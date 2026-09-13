use super::*;

pub(super) struct PostgresTriggerOccurrenceRetentionFaultInjector {
    pub(super) pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl lash_conformance::TriggerOccurrenceRetentionFaultInjector
    for PostgresTriggerOccurrenceRetentionFaultInjector
{
    async fn fail_occurrence_delete(&self, occurrence_id: &str) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION lash_fig1507_fail_occurrence_delete()
             RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN
                 RAISE EXCEPTION 'injected FIG-1507 occurrence delete failure';
             END;
             $$",
        )
        .execute(&self.pool)
        .await
        .expect("create Postgres occurrence delete failure function");
        let occurrence_id = occurrence_id.replace('\'', "''");
        sqlx::query(&format!(
            "CREATE TRIGGER fail_fig1507_occurrence_delete
             BEFORE DELETE ON lash_trigger_occurrences
             FOR EACH ROW WHEN (OLD.occurrence_id = '{occurrence_id}')
             EXECUTE FUNCTION lash_fig1507_fail_occurrence_delete()"
        ))
        .execute(&self.pool)
        .await
        .expect("install Postgres occurrence delete failure trigger");
    }

    async fn clear_occurrence_delete_failure(&self) {
        sqlx::query(
            "DROP TRIGGER IF EXISTS fail_fig1507_occurrence_delete ON lash_trigger_occurrences",
        )
        .execute(&self.pool)
        .await
        .expect("clear Postgres occurrence delete failure trigger");
        sqlx::query("DROP FUNCTION IF EXISTS lash_fig1507_fail_occurrence_delete()")
            .execute(&self.pool)
            .await
            .expect("clear Postgres occurrence delete failure function");
    }
}

struct PostgresTriggerOccurrenceListingFaultInjector {
    pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl lash_conformance::TriggerOccurrenceListingFaultInjector
    for PostgresTriggerOccurrenceListingFaultInjector
{
    async fn insert_malformed_occurrence(&self) {
        sqlx::query(
            "INSERT INTO lash_trigger_occurrences (
                occurrence_id, idempotency_key, source_type, source_key,
                occurred_at_ms, record_json
             ) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind("occurrence-listing-malformed")
        .bind("occurrence-listing-malformed")
        .bind("ui.button.pressed")
        .bind("occurrence-listing-malformed-source")
        .bind(0_i64)
        .bind("{not valid json")
        .execute(&self.pool)
        .await
        .expect("insert malformed Postgres occurrence");
    }

    async fn make_occurrence_query_unavailable(&self) {
        self.pool.close().await;
    }
}

lash_conformance::trigger_occurrence_listing_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres trigger occurrence-listing corruption law: database is not configured"
        );
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    let store = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
    let injector = Arc::new(PostgresTriggerOccurrenceListingFaultInjector { pool });
    (database_lock, store, injector)
});
