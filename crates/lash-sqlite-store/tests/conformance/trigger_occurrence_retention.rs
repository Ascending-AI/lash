//! Failure-report proof for SQLite trigger-occurrence retention.

use std::sync::Arc;

use lash_conformance::TriggerOccurrenceRetentionFaultInjector;
use lash_core_execution::{ProcessRegistry, TriggerStore};
use lash_sqlite_store::SqliteDatabase;

use super::{Retained, SUBSTRATE};
use crate::backend_fixture::TestBackend;

struct SqliteTriggerOccurrenceRetentionFaultInjector {
    backend: TestBackend,
}

#[async_trait::async_trait]
impl TriggerOccurrenceRetentionFaultInjector for SqliteTriggerOccurrenceRetentionFaultInjector {
    async fn fail_occurrence_delete(&self, occurrence_id: &str) {
        let conn = self.backend.raw(SqliteDatabase::Triggers);
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
        let conn = self.backend.raw(SqliteDatabase::Triggers);
        conn.execute_batch("DROP TRIGGER IF EXISTS fail_fig1507_occurrence_delete")
            .expect("clear SQLite occurrence delete failure trigger");
    }
}

lash_conformance::trigger_retention_fault_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.trigger_store() as Arc<dyn TriggerStore>;
    let fault = Arc::new(SqliteTriggerOccurrenceRetentionFaultInjector {
        backend: backend.clone(),
    });
    (backend, store, fault)
});

lash_conformance::process_trigger_retention_tests!({
    let retained = Retained::default();
    ((), move || {
        let retained = retained.clone();
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            lash_conformance::ProcessTriggerRetentionHandles {
                registry: backend.process_registry() as Arc<dyn ProcessRegistry>,
                triggers: backend.trigger_store() as Arc<dyn TriggerStore>,
                sessions: backend.session_store_factory()
                    as Arc<dyn lash_core_execution::SessionStoreFactory>,
            }
        }
    })
});
