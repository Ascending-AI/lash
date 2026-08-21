//! Failure-report proof for SQLite trigger-occurrence retention.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lash_core::testing::conformance::{
    LegacyTriggerMutationReceiptInjector, TriggerOccurrenceRetentionFaultInjector,
};
use lash_core::{ProcessRegistry, TriggerStore};
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::{SqliteProcessRegistry, SqliteSessionStoreFactory, SqliteTriggerStore};

struct SqliteTriggerOccurrenceRetentionFaultInjector {
    path: PathBuf,
}

struct SqliteLegacyTriggerMutationReceiptInjector {
    path: PathBuf,
}

#[async_trait::async_trait]
impl LegacyTriggerMutationReceiptInjector for SqliteLegacyTriggerMutationReceiptInjector {
    async fn insert_legacy_receipt(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
        result_json: &str,
        created_at_ms: u64,
    ) {
        let conn = rusqlite::Connection::open(&self.path)
            .expect("open SQLite legacy trigger receipt fixture");
        conn.execute(
            "INSERT INTO trigger_mutation_receipts (
                operation_id, request_fingerprint, result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                operation_id,
                request_fingerprint,
                result_json,
                i64::try_from(created_at_ms).expect("legacy receipt timestamp fits SQLite")
            ],
        )
        .expect("insert SQLite legacy trigger receipt");
    }

    async fn receipt_exists(&self, operation_id: &str) -> bool {
        let conn = rusqlite::Connection::open(&self.path)
            .expect("open SQLite legacy trigger receipt fixture");
        conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM trigger_mutation_receipts WHERE operation_id = ?1
             )",
            rusqlite::params![operation_id],
            |row| row.get(0),
        )
        .expect("inspect SQLite legacy trigger receipt")
    }
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

    async fn clear_occurrence_delete_failure(&self) {
        let conn = rusqlite::Connection::open(&self.path)
            .expect("open SQLite trigger occurrence failure injector");
        conn.execute_batch("DROP TRIGGER IF EXISTS fail_fig1507_occurrence_delete")
            .expect("clear SQLite occurrence delete failure trigger");
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

#[tokio::test]
async fn sqlite_trigger_retention_reconciliation_is_transactional() {
    let dir = tempfile::tempdir().expect("SQLite trigger retention tempdir");
    let path = dir.path().join("trigger-reconciliation.db");
    let store = super::open_trigger_store(&path);
    let fault = SqliteTriggerOccurrenceRetentionFaultInjector { path };
    lash_core::testing::conformance::trigger_retention_reconciliation_failure_law(store, &fault)
        .await;
}

#[tokio::test]
async fn sqlite_legacy_ownerless_trigger_receipt_is_retained() {
    let dir = tempfile::tempdir().expect("SQLite legacy trigger receipt tempdir");
    let path = dir.path().join("legacy-trigger-receipt.db");
    let store = super::open_trigger_store(&path);
    let injector = SqliteLegacyTriggerMutationReceiptInjector { path };
    lash_core::testing::conformance::legacy_ownerless_trigger_receipt_is_retained_law(
        store, &injector,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_trigger_retention_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::process_trigger_retention(move || {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("process-trigger retention tempdir");
            let sessions_root = dir.path().join("sessions");
            let registry = Arc::new(
                SqliteProcessRegistry::open(
                    &dir.path().join("processes.db"),
                    sessions_root.clone(),
                )
                .await
                .expect("process registry"),
            ) as Arc<dyn ProcessRegistry>;
            let triggers = Arc::new(
                SqliteTriggerStore::open(&dir.path().join("triggers.db"))
                    .await
                    .expect("trigger store"),
            ) as Arc<dyn TriggerStore>;
            let sessions = Arc::new(SqliteSessionStoreFactory::new(sessions_root))
                as Arc<dyn lash_core::SessionStoreFactory>;
            dirs.lock_recover().push(dir);
            lash_core::testing::conformance::ProcessTriggerRetentionHandles {
                registry,
                triggers,
                sessions,
            }
        }
    })
    .await;
}
