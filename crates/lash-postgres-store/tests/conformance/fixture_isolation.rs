use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_fixtures_do_not_hold_the_shared_database_lock() {
    let Some((_guard, storage)) = storage().await else {
        return;
    };
    let mut connection = storage
        .pool()
        .acquire()
        .await
        .expect("acquire lock probe connection");
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock_shared($1)")
        .bind(0x4c41_5348_5f50_4754_i64)
        .fetch_one(&mut *connection)
        .await
        .expect("probe the superseded fixture lock");
    assert!(
        acquired,
        "a conformance fixture still serializes unrelated laws"
    );
    sqlx::query("SELECT pg_advisory_unlock_shared($1)")
        .bind(0x4c41_5348_5f50_4754_i64)
        .execute(&mut *connection)
        .await
        .expect("release the lock probe");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_resets_and_reopens_stay_in_the_laws_database() {
    let Some((_first_guard, first)) = storage().await else {
        return;
    };
    let mut connection = first.pool().acquire().await.expect("acquire fixture probe");
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock_shared($1)")
        .bind(0x4c41_5348_5f50_4754_i64)
        .fetch_one(&mut *connection)
        .await
        .expect("probe fixture independence before creating its companion");
    assert!(
        acquired,
        "the first fixture prevents a companion law from opening"
    );
    sqlx::query("SELECT pg_advisory_unlock_shared($1)")
        .bind(0x4c41_5348_5f50_4754_i64)
        .execute(&mut *connection)
        .await
        .expect("release fixture probe");
    drop(connection);
    let (_second_guard, second) = storage().await.expect("open companion fixture");
    assert_ne!(first.catalog_id(), second.catalog_id());
    let databases = vec![
        _first_guard.database_name().to_owned(),
        _second_guard.database_name().to_owned(),
    ];
    assert_ne!(databases[0], databases[1]);
    for (storage, body) in [(&first, vec![1_u8]), (&second, vec![2_u8])] {
        sqlx::query("INSERT INTO lash_blobs (hash, content) VALUES ($1, $2)")
            .bind("same-law-fixture-key")
            .bind(body)
            .execute(storage.pool())
            .await
            .expect("insert the same key in independent laws");
    }
    reset(second.pool()).await;
    let reopened = lash_postgres_store::testing::connect(_first_guard.url())
        .await
        .expect("reopen the first law on an independent pool");
    assert_eq!(reopened.catalog_id(), first.catalog_id());
    let body: Vec<u8> = sqlx::query_scalar("SELECT content FROM lash_blobs WHERE hash = $1")
        .bind("same-law-fixture-key")
        .fetch_one(reopened.pool())
        .await
        .expect("companion reset must preserve the first law's durable row");
    assert_eq!(body, vec![1]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_blobs")
        .fetch_one(second.pool())
        .await
        .expect("read companion reset");
    assert_eq!(count, 0);
    let mut held_read = first
        .pool()
        .begin()
        .await
        .expect("hold law read transaction");
    sqlx::query("SELECT count(*) FROM lash_blobs")
        .execute(&mut *held_read)
        .await
        .expect("hold a database read lock through fixture teardown");
    reopened.pool().close().await;
    drop((_first_guard, _second_guard));
    drop(held_read);
    first.pool().close().await;
    second.pool().close().await;
    let cleanup = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url().expect("configured fixture database"))
        .await
        .expect("connect database cleanup probe");
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname = ANY($1)")
            .bind(&databases)
            .fetch_one(&cleanup)
            .await
            .expect("probe completed database teardown");
    assert_eq!(remaining, 0, "a law left its database behind");
    cleanup.close().await;
}
