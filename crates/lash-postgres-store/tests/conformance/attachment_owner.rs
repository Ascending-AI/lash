use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_attachment_owner_cold_replay_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres attachment-owner conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let storage = Arc::new(storage);
    let scope = durable_turn_scope("attachment-owner-cold-replay", "attachment-owner-turn");
    let first = Arc::new(storage.runtime_effect_controller(scope.clone()))
        as Arc<dyn lash_core::RuntimeEffectController>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let reopen_effect_controller = {
        let storage = Arc::clone(&storage);
        Arc::new(move || {
            let controller = Arc::new(storage.runtime_effect_controller(scope.clone()))
                as Arc<dyn lash_core::RuntimeEffectController>;
            Box::pin(async move { controller })
                as std::pin::Pin<
                    Box<dyn Future<Output = Arc<dyn lash_core::RuntimeEffectController>> + Send>,
                >
        })
    };
    let clock = Arc::new(lash_core::testing::TestClock::new(
        lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock)
            .saturating_sub(100_000),
    ));
    let factory = Arc::new(
        storage
            .session_store_factory_with_shared_process_registry()
            .with_clock(clock.clone()),
    ) as Arc<dyn SessionStoreFactory>;
    let advance_clock = {
        let clock = Arc::clone(&clock);
        Arc::new(move |duration_ms| clock.advance(duration_ms)) as Arc<dyn Fn(u64) + Send + Sync>
    };

    lash_conformance::attachment_owner_cold_replay(
        lash_conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store: Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
    .await;
}
