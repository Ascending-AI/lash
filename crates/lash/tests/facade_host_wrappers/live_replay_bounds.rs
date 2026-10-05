use super::*;
use lash::observe::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayGapReason, LiveReplayStore,
    SessionResume,
};

async fn eviction_law(backend: lash::Backend, tag: &str) -> LashCore {
    let replay = Arc::new(InMemoryLiveReplayStore::new(
        InMemoryLiveReplayStoreConfig {
            max_sessions: 1,
            ..InMemoryLiveReplayStoreConfig::default()
        },
    ));
    let provider = lash::testing::TestProvider::builder()
        .kind("live-replay-bounds")
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "retained turn".into(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    let core = LashCore::standard_builder(backend)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "live-replay-bounds",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("live-replay-bounds")
                            .context_window_tokens(64_000)
                            .build()
                            .expect("model"),
                        provider,
                    ),
                )
                .expect("one key registers"),
        ))
        .live_replay_store(replay.clone())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "replay-bounds",
            tag,
        ))
        .expect("core");
    let id = format!("replay-bounds-{tag}");
    core.session(lash::SessionId::fixture(&id))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "live-replay-bounds",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create");
    let session = core
        .session(lash::SessionId::fixture(&id))
        .open()
        .await
        .expect("open");
    let old = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let output = session
        .send(TurnInput::text("publish before eviction"))
        .output()
        .await
        .expect("turn");
    assert!(output.is_success(), "{output:?}");
    let committed = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");
    assert!(
        matches!(session.observe().resume_from_cursor(&old).await.expect("retained replay"), SessionResume::Replayed { events } if !events.is_empty())
    );

    replay.current_cursor(
        &lash::SessionId::fixture(format!("pressure-{tag}")),
        lash::observe::SessionRevision::new(0),
    );
    let SessionResume::Gap { observation, gap } = session
        .observe()
        .resume_from_cursor(&old)
        .await
        .expect("eviction recovery")
    else {
        panic!("evicted cursor must recover through a gap");
    };
    assert_eq!(gap.reason, LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_eq!(
        observation.read_view.turn_index(),
        committed.read_view.turn_index()
    );
    assert_eq!(
        observation.read_view.messages().len(),
        committed.read_view.messages().len()
    );
    assert_ne!(observation.cursor, old);
    assert!(matches!(
        replay.replay_after_cursor(&old).await,
        Ok(lash::persistence::LiveReplayOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
    assert!(matches!(
        replay.subscribe_after_cursor(&old).await,
        Ok(lash::observe::LiveReplaySubscribeOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
    core
}

async fn on_double(tier: Tier, seed: u64, tag: &str) {
    let Some(double) = tiers::double(tier, seed, |stores| stores).await else {
        return;
    };
    eviction_law(double.double.lash_backend(), tag)
        .await
        .shutdown()
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn eviction_recovers_the_facade_on_sqlite_memory() {
    on_double(Tier::SqliteMemory, 0x4295_0001, "sqlite-memory").await;
}

#[tokio::test]
async fn eviction_recovers_the_facade_on_sqlite_file() {
    on_double(Tier::SqliteFile, 0x4295_0002, "sqlite-file").await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn eviction_recovers_the_facade_on_postgres() {
    on_double(Tier::Postgres, 0x4295_0003, "postgres").await;
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} must be configured by the live-replay-bounds Restate suite")
    })
}

async fn on_live(postgres: bool) {
    use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
    let tag = uuid::Uuid::new_v4().to_string();
    let config = LiveConfig {
        ingress_url: env("RESTATE_INGRESS_URL"),
        admin_url: env("RESTATE_ADMIN_URL"),
        endpoint_bind: env("REPLAY_BOUNDS_BIND").parse().expect("endpoint bind"),
        endpoint_url: env("REPLAY_BOUNDS_URL"),
        run_tag: tag.clone(),
        namespace: lash::restate::RestateNamespace::new(format!("replay-bounds-{tag}"))
            .expect("namespace"),
    };
    if postgres {
        let database = lash_postgres_store::testing::IsolatedDatabase::create(&env(
            "LASH_POSTGRES_DATABASE_URL",
        ))
        .await;
        let storage = lash_postgres_store::PostgresStorage::connect(database.url())
            .await
            .expect("PostgreSQL storage");
        let attachments = tempfile::tempdir().expect("attachments");
        let live = LiveRestateBackend::start_with_store_set(config, |clock| async {
            Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    attachments.path(),
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock,
            )) as Arc<dyn lash::StoreSet>)
        })
        .await
        .expect("native Restate PostgreSQL backend");
        let core = eviction_law(live.lash_backend(), &tag).await;
        live.finish().await;
        core.shutdown().await.expect("shutdown");
    } else {
        let directory = tempfile::tempdir().expect("SQLite directory");
        let live = LiveRestateBackend::start_with_store_set(config, |clock| async {
            let stores =
                lash_sqlite_store::SqliteStoreSet::open_with_clock(directory.path(), clock)
                    .await
                    .expect("SQLite file storage");
            Ok(Arc::new(stores) as Arc<dyn lash::StoreSet>)
        })
        .await
        .expect("native Restate SQLite backend");
        let core = eviction_law(live.lash_backend(), &tag).await;
        live.finish().await;
        core.shutdown().await.expect("shutdown");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the live-replay-bounds Restate suite"]
async fn eviction_recovers_the_facade_on_native_restate_sqlite() {
    on_live(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the live-replay-bounds Restate suite and PostgreSQL"]
async fn eviction_recovers_the_facade_on_native_restate_postgres() {
    on_live(true).await;
}
