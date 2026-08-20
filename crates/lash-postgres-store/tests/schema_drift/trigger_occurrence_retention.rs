//! Schema migration proofs for trigger-occurrence reclaim eligibility.

use lash_postgres_store::{PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaProvisioning};

use crate::harness::{REWIND_PAST_56_ARTIFACTS, ScratchSchema};
use crate::support::database_url;

/// The immediate predecessor adds one nullable arming column and its partial
/// index when the occurrence scope is empty.
#[tokio::test]
async fn main_component_55_store_upgrades_cleanly_to_56() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-55 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_56_ARTIFACTS}
             UPDATE lash_schema_versions
                SET version = 55
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("the exact published component-55 shape migrates to 56");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 56);
    let column_present: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name = 'lash_trigger_occurrences'
               AND column_name = 'reclaimable_at_ms'
         )",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated occurrence eligibility column");
    assert!(column_present);
    let index_present: bool =
        sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
            .bind("idx_lash_trigger_occurrences_reclaimable")
            .fetch_one(&scratch.pool)
            .await
            .expect("read migrated occurrence eligibility index");
    assert!(index_present);
    scratch.cleanup().await;
}

/// Eligibility must be armed by ingest or final-delivery terminality. A schema
/// migration may not infer it from old row shape, and carrying those rows
/// forward as permanently unarmed would leak them, so a populated predecessor
/// refuses without changing either its stamp or table.
#[tokio::test]
async fn populated_component_55_trigger_scope_refuses_the_56_migration() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping populated component-55 refusal law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_56_ARTIFACTS}
             INSERT INTO lash_trigger_occurrences (
                 occurrence_id, idempotency_key, source_type, source_key,
                 occurred_at_ms, record_json
             ) VALUES (
                 'legacy-occurrence', 'legacy-idempotency', 'legacy', 'legacy',
                 1, '{{}}'
             );
             UPDATE lash_schema_versions
                SET version = 55
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    let error = PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await;
    let error = match error {
        Ok(_) => panic!("a populated trigger scope cannot cross the eligibility cutover"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("component 56 requires an empty lash_trigger_occurrences table"),
        "the refusal must name the empty-scope precondition: {error}"
    );
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read refused component version");
    assert_eq!(version, 55);
    let occurrence_count: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_trigger_occurrences")
        .fetch_one(&scratch.pool)
        .await
        .expect("read preserved predecessor occurrence");
    assert_eq!(occurrence_count, 1);
    scratch.cleanup().await;
}
