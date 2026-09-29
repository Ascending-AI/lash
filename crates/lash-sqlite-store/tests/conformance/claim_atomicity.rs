use lash_core_execution::{RuntimeStore, SessionCatalogStore};
use lash_sqlite_store::SqliteDatabase;
use std::sync::Arc;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;
#[path = "../../../lash-core/tests/support/queued_claim_atomicity.rs"]
mod law;

#[tokio::test]
async fn sqlite_a_partial_admission_rolls_back_through_both_entry_points() {
    for entry in law::ENTRIES {
        let backend = TestBackend::open(SUBSTRATE).await;
        let store = backend.store().await;
        store
            .admit_session(&lash_core_execution::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: "root".into(),
                relation: lash_core_execution::SessionRelation::Root,
                policy: lash_core_execution::SessionPolicy::new(
                    lash_core_execution::TurnBudget::Unbounded,
                ),
            })
            .await
            .unwrap();
        let case = law::prepare(store as Arc<dyn RuntimeStore>, entry).await;
        let conn = backend.raw(SqliteDatabase::DurableCore);
        let second = case.ids[1].replace('\'', "''");
        conn.execute_batch(&format!("CREATE TRIGGER lose_second_bind BEFORE UPDATE OF admitted_root ON queued_work_batches WHEN OLD.batch_id = '{second}' BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            case.admit().await.is_err(),
            "{entry:?}: a partial admission is refused"
        );
        let bound: i64 = conn
            .query_row(
                "SELECT count(*) FROM queued_work_batches WHERE admitted_root IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound, 0, "{entry:?}: the first row's bind must roll back");
        conn.execute_batch("DROP TRIGGER lose_second_bind").unwrap();
        assert_eq!(
            case.admit().await.unwrap().len(),
            2,
            "{entry:?}: both rows remain admissible"
        );
    }
}

#[tokio::test]
async fn sqlite_an_admission_holds_its_rows_across_a_displaced_fence() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    store
        .admit_session(&lash_core_execution::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: "root".into(),
            relation: lash_core_execution::SessionRelation::Root,
            policy: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            ),
        })
        .await
        .unwrap();
    law::an_admission_holds_its_rows_across_a_displaced_fence(
        store as Arc<dyn RuntimeStore>,
        "sqlite",
    )
    .await;
}
