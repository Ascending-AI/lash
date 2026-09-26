use super::{pg_law_backend, reset, storage};

lash_conformance::session_close_tests!({
    let Some((lock, storage)) = storage().await else {
        panic!("Postgres session-close laws require LASH_POSTGRES_DATABASE_URL");
    };
    reset(storage.pool()).await;
    let (guard, backend) = pg_law_backend(&storage).await;
    (
        (lock, guard),
        "postgres-session-close",
        backend.effect_host(),
        backend.stores(),
        None,
    )
});
