//! Component 118 (FIG-3544): every pending turn input carries an immutable
//! submitted ingress and submission digest written once at admission, and
//! source-key replay compares only the digest.
//!
//! A component-117 catalog holds rows with neither column, and the digest is a
//! Rust-computed value no DDL can backfill, so the boundary is destructive: the
//! whole store is refused at open rather than replayed against a missing
//! digest.

use lash_postgres_store::PostgresStorage;

use crate::support::{SharedDatabaseLock, database_url};

const PRE_SUBMISSION_DIGEST_COMPONENT_VERSION: i32 = 117;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_refuses_pre_submission_digest_catalog_at_open() {
    let Some(url) = database_url() else {
        eprintln!("skipping Postgres pre-submission-digest open gate: database is not configured");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    let pool = storage.pool().clone();
    assert!(
        PostgresStorage::schema_version() > PRE_SUBMISSION_DIGEST_COMPONENT_VERSION,
        "this gate pins the boundary component 118 introduced, which every later \
         component keeps"
    );

    // The component-117 shape of the pending-input table, stamped as 117.
    sqlx::query(
        "ALTER TABLE lash_pending_turn_inputs
             DROP COLUMN submitted_ingress_json,
             DROP COLUMN submission_digest",
    )
    .execute(&pool)
    .await
    .expect("rewrite the pending-input table to its component-117 shape");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = $1 WHERE component = 'lash-postgres-store'",
    )
    .bind(PRE_SUBMISSION_DIGEST_COMPONENT_VERSION)
    .execute(&pool)
    .await
    .expect("stamp the pre-submission-digest component schema");

    let result = PostgresStorage::from_pool(pool.clone()).await;

    sqlx::query(
        "ALTER TABLE lash_pending_turn_inputs
             ADD COLUMN submitted_ingress_json TEXT NOT NULL DEFAULT '',
             ADD COLUMN submission_digest TEXT NOT NULL DEFAULT ''",
    )
    .execute(&pool)
    .await
    .expect("restore the current pending-input columns");
    sqlx::query(
        "ALTER TABLE lash_pending_turn_inputs
             ALTER COLUMN submitted_ingress_json DROP DEFAULT,
             ALTER COLUMN submission_digest DROP DEFAULT",
    )
    .execute(&pool)
    .await
    .expect("restore the current pending-input column shape");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = $1 WHERE component = 'lash-postgres-store'",
    )
    .bind(PostgresStorage::schema_version())
    .execute(&pool)
    .await
    .expect("restore current component schema");

    let message = match result {
        Ok(_) => panic!("a pre-submission-digest catalog must be refused at open"),
        Err(error) => error.to_string(),
    };
    let expected = PostgresStorage::schema_version();
    assert!(
        message.contains(&format!(
            "has version {PRE_SUBMISSION_DIGEST_COMPONENT_VERSION}, expected {expected}"
        )),
        "the refusal names both generations: {message}"
    );
    assert!(
        message.contains("has no applicable migration"),
        "component 118 is a reject-and-recreate boundary: {message}"
    );
    assert!(
        message.contains(&format!(
            "This build declares no forward migration into component {expected}"
        )),
        "no arm reaches the destructive cutover: {message}"
    );
}
