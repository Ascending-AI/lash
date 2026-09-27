use super::{double_law_backend, reset, storage};

lash_conformance::session_close_tests!({
    let Some((lock, storage)) = storage().await else {
        panic!("Postgres session-close laws require LASH_POSTGRES_DATABASE_URL");
    };
    reset(storage.pool()).await;
    // The close runs as attempts of the engine's `SessionDelete` handler:
    // the double lends the law's close the handler-scoped controller a
    // Restate tier actually runs it on, over this test's PostgreSQL stores.
    let (guard, stores, host, runner) = double_law_backend(&storage).await;
    (
        (lock, guard),
        "postgres-session-close",
        host,
        stores,
        Some(runner),
    )
});
