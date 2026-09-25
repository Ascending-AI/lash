use super::*;

lash_conformance::attachment_owner_cold_replay_tests!({
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres attachment-owner conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let storage = Arc::new(storage);
    let scope = durable_turn_scope("attachment-owner-cold-replay", "attachment-owner-turn");
    // PostgreSQL journals no effects (ADR 0104): the law's controller is the
    // promise authority's, over a journal file the reopen shares.
    let promise_dir = tempfile::tempdir().expect("promise authority directory");
    let journal = promise_dir.path().join("promises.db");
    let first = Arc::new(
        lash_sqlite_store::SqliteRuntimeEffectController::open(&journal, scope.clone())
            .await
            .expect("first effect controller"),
    ) as Arc<dyn lash_core_execution::RuntimeEffectController>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let reopen_effect_controller = {
        Arc::new(move || {
            let journal = journal.clone();
            let scope = scope.clone();
            Box::pin(async move {
                Arc::new(
                    lash_sqlite_store::SqliteRuntimeEffectController::open(&journal, scope)
                        .await
                        .expect("cold replay effect controller"),
                ) as Arc<dyn lash_core_execution::RuntimeEffectController>
            })
                as std::pin::Pin<
                    Box<
                        dyn Future<Output = Arc<dyn lash_core_execution::RuntimeEffectController>>
                            + Send,
                    >,
                >
        })
    };
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        lash_core_execution::ClockWallTime::timestamp_ms(
            &lash_core_execution::facade_support::SystemClock,
        )
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

    // PostgreSQL keeps no attachment bytes: a deployment pairs it with a byte
    // store of its own, here a filesystem one.
    let attachments = tempfile::tempdir().expect("attachment directory");
    let attachment_store =
        Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(attachments.path()));
    (
        (_database_lock, attachments, promise_dir),
        lash_conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store,
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
});
