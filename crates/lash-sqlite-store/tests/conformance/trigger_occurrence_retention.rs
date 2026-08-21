//! Failure-report proof for SQLite trigger-occurrence retention.

use std::path::PathBuf;

use lash_core::testing::conformance::TriggerOccurrenceRetentionFaultInjector;

struct SqliteTriggerOccurrenceRetentionFaultInjector {
    path: PathBuf,
}

#[async_trait::async_trait]
impl TriggerOccurrenceRetentionFaultInjector for SqliteTriggerOccurrenceRetentionFaultInjector {
    async fn fail_occurrence_delete(&self, occurrence_id: &str) {
        let conn = rusqlite::Connection::open(&self.path)
            .expect("open SQLite trigger occurrence failure injector");
        let occurrence_id = occurrence_id.replace('\'', "''");
        conn.execute_batch(&format!(
            "CREATE TRIGGER fail_fig1507_occurrence_delete
             BEFORE DELETE ON trigger_occurrences
             WHEN OLD.occurrence_id = '{occurrence_id}'
             BEGIN
                 SELECT RAISE(FAIL, 'injected FIG-1507 occurrence delete failure');
             END;"
        ))
        .expect("install SQLite occurrence delete failure trigger");
    }
}

#[tokio::test]
async fn sqlite_trigger_occurrence_retention_failure_is_not_laundered() {
    let dir = tempfile::tempdir().expect("SQLite trigger retention tempdir");
    let path = dir.path().join("trigger-retention.db");
    let store = super::open_trigger_store(&path);
    let fault = SqliteTriggerOccurrenceRetentionFaultInjector { path };
    lash_core::testing::conformance::trigger_occurrence_retention_failure_law(store, &fault).await;
}
