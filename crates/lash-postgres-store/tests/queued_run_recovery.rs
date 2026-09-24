use lash_core_execution::llm::types::{LlmRequest, LlmResponse};
use lash_core_execution::{LlmOutputPart, TurnInput};
use lash_postgres_store::PostgresStorage;
use lash_sansio::sync::MutexExt;
use sqlx::Row;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

struct RetryProbe {
    provider_calls: AtomicUsize,
    hook_calls: AtomicUsize,
    hook_entered: tokio::sync::Notify,
    hook_release: tokio::sync::Semaphore,
    requests: StdMutex<Vec<LlmRequest>>,
}

impl Default for RetryProbe {
    fn default() -> Self {
        Self {
            provider_calls: AtomicUsize::new(0),
            hook_calls: AtomicUsize::new(0),
            hook_entered: tokio::sync::Notify::new(),
            hook_release: tokio::sync::Semaphore::new(0),
            requests: StdMutex::new(Vec::new()),
        }
    }
}

struct RetryHook(Arc<RetryProbe>);

impl lash_core_execution::facade_support::PluginFactory for RetryHook {
    fn id(&self) -> &'static str {
        "queued-run-retry"
    }

    fn build(
        &self,
        _: &lash_core_execution::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core_execution::facade_support::SessionPlugin>,
        lash_core_execution::PluginError,
    > {
        Ok(Arc::new(Self(Arc::clone(&self.0))))
    }
}

impl lash_core_execution::facade_support::SessionPlugin for RetryHook {
    fn id(&self) -> &'static str {
        "queued-run-retry"
    }

    fn register(
        &self,
        reg: &mut lash_core_execution::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core_execution::PluginError> {
        let probe = Arc::clone(&self.0);
        reg.output().response(Arc::new(move |context| {
            let probe = Arc::clone(&probe);
            Box::pin(async move {
                let attempt = probe.hook_calls.fetch_add(1, Ordering::SeqCst);
                probe.hook_entered.notify_one();
                probe
                    .hook_release
                    .acquire()
                    .await
                    .expect("hook barrier remains open")
                    .forget();
                if attempt == 0 {
                    return Err(lash_core_execution::PluginError::Invoke(
                        "transient response derivation".into(),
                    ));
                }
                Ok(
                    lash_core_execution::facade_support::AssistantResponseTransform {
                        response: context.response,
                        events: Vec::new(),
                    },
                )
            })
        }));
        Ok(())
    }
}

#[expect(
    clippy::expect_used,
    reason = "fixture query and journal decoding must succeed to inspect recorded provider effects"
)]
async fn recorded_provider_effects(
    storage: &PostgresStorage,
) -> Vec<(String, String, serde_json::Value)> {
    let rows = sqlx::query("SELECT scope_id, replay_key, envelope_json FROM lash_runtime_effect_replay WHERE outcome_json IS NOT NULL ORDER BY replay_key")
        .fetch_all(storage.pool())
        .await
        .expect("read recorded journal effects");
    rows.into_iter()
        .map(|row| {
            let payload: String = row.get(2);
            let wrapped: serde_json::Value =
                serde_json::from_str(&payload).expect("decode journal envelope wrapper");
            let envelope = serde_json::from_str::<serde_json::Value>(
                wrapped["json"]
                    .as_str()
                    .expect("journal envelope JSON string"),
            )
            .expect("decode journal envelope");
            (row.get(0), row.get(1), envelope)
        })
        .filter(|(_, _, envelope)| envelope["command"]["type"] == "llm_call")
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automatic_queued_retry_reuses_recorded_completion_before_new_arrivals()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(database_url) = super::support::database_url() else {
        return Ok(());
    };
    let _lock = super::support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url).await?;
    super::support::reset(storage.pool()).await;
    let probe = Arc::new(RetryProbe::default());
    let provider = lash::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let probe = Arc::clone(&probe);
            move |request| {
                let probe = Arc::clone(&probe);
                async move {
                    probe.provider_calls.fetch_add(1, Ordering::SeqCst);
                    probe.requests.lock_recover().push(request);
                    Ok(text_response("recorded completion"))
                }
            }
        })
        .build()
        .into_handle();
    let attachments = tempfile::tempdir().expect("attachment root");
    let backend = Arc::new(lash_postgres_store::PostgresBackend::new(
        &storage,
        Arc::new(lash::persistence::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("embed-test")
                .context_window_tokens(4096)
                .build()
                .unwrap(),
        )
        .plugin(Arc::new(RetryHook(Arc::clone(&probe))))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .native_substrate_config(lash_core_execution::NativeSubstrateConfig {
            work_cadence: lash_core_execution::WorkCadencePolicy {
                retry_initial: std::time::Duration::from_millis(50),
                retry_max: std::time::Duration::from_millis(50),
                ..Default::default()
            },
            ..Default::default()
        })
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "pg-queued-retry",
            "pg-queued-retry-boot",
        ))?;
    let session = core.session("automatic-queued-retry").open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("first admitted input"))
        .id("first")
        .send()
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.hook_entered.notified(),
    )
    .await
    .expect("automatic scheduler reaches first hook");
    assert_eq!(probe.provider_calls.load(Ordering::SeqCst), 1);
    let first_journal = recorded_provider_effects(&storage).await;
    assert_eq!(
        first_journal.len(),
        1,
        "phase 1 is durable before the hook fails"
    );
    session
        .durable()
        .enqueue(TurnInput::text("later arrival"))
        .id("later")
        .send()
        .await?;
    probe.hook_release.add_permits(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.hook_entered.notified(),
    )
    .await
    .expect("automatic scheduler retries the actual response hook");
    assert_eq!(
        probe.hook_calls.load(Ordering::SeqCst),
        2,
        "the hook recovers after its transient failure"
    );
    assert_eq!(
        probe.provider_calls.load(Ordering::SeqCst),
        1,
        "retry must reuse the recorded completion at the admitted physical position"
    );
    assert_eq!(
        recorded_provider_effects(&storage).await,
        first_journal,
        "retry reaches phase 2 with the same recorded scope, replay key and physical attribution"
    );
    let first_request = serde_json::to_string(&probe.requests.lock_recover()[0].messages).unwrap();
    assert!(first_request.contains("first admitted input"));
    assert!(!first_request.contains("later arrival"));
    probe.hook_release.add_permits(16);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if session
                .durable()
                .pending_turn_inputs()
                .await
                .expect("pending inputs")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "both admitted runs settle: {error}; provider calls {}",
            probe.provider_calls.load(Ordering::SeqCst)
        )
    });
    assert_eq!(
        probe.provider_calls.load(Ordering::SeqCst),
        2,
        "a distinct run executes independently"
    );
    session
        .durable()
        .enqueue(TurnInput::text("distinct submission"))
        .id("distinct")
        .send()
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if probe.provider_calls.load(Ordering::SeqCst) == 3
                && session
                    .durable()
                    .pending_turn_inputs()
                    .await
                    .unwrap()
                    .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new submission settles independently");
    let journal = recorded_provider_effects(&storage).await;
    assert_eq!(journal.len(), 3);
    assert_eq!(
        journal
            .iter()
            .map(|entry| &entry.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "distinct runs have distinct admission identities"
    );
    Ok(())
}
