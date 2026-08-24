//! Schema migration proofs for trigger-occurrence reclaim eligibility.

use lash_core::{TriggerDeliveryRetentionCandidate, TriggerStore};
use lash_postgres_store::{PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaProvisioning};

use crate::harness::{REWIND_PAST_55_ARTIFACTS, ScratchSchema};
use crate::support::database_url;

/// The immediate predecessor adds one nullable arming column and its partial
/// index when the occurrence scope is empty.
#[tokio::test]
async fn main_component_55_store_upgrades_cleanly_to_57() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-55 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_55_ARTIFACTS}
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
    .expect("the exact published component-55 shape migrates to 60");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 60);
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

/// A populated predecessor migrates by witnessing the same terminal data
/// predicate as the runtime: zero-fan-out rows arm from their occurrence time,
/// while live-fan-out rows stay unarmed until the final delivery is deleted.
#[tokio::test]
async fn populated_component_55_trigger_scope_arms_only_terminal_rows() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping populated component-55 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_55_ARTIFACTS}
             INSERT INTO lash_trigger_occurrences (
                 occurrence_id, idempotency_key, source_type, source_key,
                 occurred_at_ms, record_json
             ) VALUES
                 (
                     'legacy-zero-fanout', 'legacy-zero-idempotency',
                     'legacy', 'zero', 11, '{{}}'
                 ),
                 (
                     'legacy-live-fanout', 'legacy-live-idempotency',
                     'legacy', 'live', 22, '{{}}'
                 );
             INSERT INTO lash_trigger_deliveries (
                 occurrence_id, subscription_id, process_id,
                 subscription_incarnation, subscription_revision,
                 subscription_snapshot_json, created_at_ms
             ) VALUES (
                 'legacy-live-fanout', 'legacy-subscription', 'legacy-process',
                 'legacy-incarnation', 1, '{{}}', 23
             );
             UPDATE lash_schema_versions
                SET version = 55
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    let storage = PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("a populated component-55 trigger scope migrates to 60");
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 60);
    let zero_fanout_arm: Option<i64> = sqlx::query_scalar(
        "SELECT reclaimable_at_ms
         FROM lash_trigger_occurrences
         WHERE occurrence_id = 'legacy-zero-fanout'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated zero-fanout occurrence arm");
    assert_eq!(zero_fanout_arm, Some(11));
    let live_fanout_arm: Option<i64> = sqlx::query_scalar(
        "SELECT reclaimable_at_ms
         FROM lash_trigger_occurrences
         WHERE occurrence_id = 'legacy-live-fanout'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated live-fanout occurrence arm");
    assert_eq!(live_fanout_arm, None);

    let trigger_store = storage.trigger_store();
    let deleted = trigger_store
        .delete_delivery_retention_candidates(&[TriggerDeliveryRetentionCandidate {
            occurrence_id: "legacy-live-fanout".to_string(),
            subscription_id: "legacy-subscription".to_string(),
            process_id: "legacy-process".to_string(),
        }])
        .await
        .expect("delete the legacy occurrence's final delivery");
    assert_eq!(deleted, 1);
    let armed_after_terminal_delete: bool = sqlx::query_scalar(
        "SELECT reclaimable_at_ms IS NOT NULL
         FROM lash_trigger_occurrences
         WHERE occurrence_id = 'legacy-live-fanout'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read live-fanout occurrence after terminal delivery delete");
    assert!(armed_after_terminal_delete);
    scratch.cleanup().await;
}
