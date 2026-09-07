use lash_core::{RuntimePersistence, SessionStoreFactory};
use lash_sqlite_store::SqliteSessionStoreFactory;
use std::sync::Arc;
#[path = "../../../lash-core/tests/support/queued_claim_atomicity.rs"]
mod law;

#[tokio::test]
async fn sqlite_queued_work_partial_claim_rolls_back_through_all_entry_points() {
    for entry in law::ENTRIES {
        let dir = tempfile::tempdir().unwrap();
        let factory = SqliteSessionStoreFactory::new(dir.path());
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: "root".into(),
                relation: lash_core::SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .unwrap();
        let case = law::prepare(store as Arc<dyn RuntimePersistence>, entry).await;
        let conn = rusqlite::Connection::open(factory.catalog_path()).unwrap();
        let second = case.ids[1].replace('\'', "''");
        conn.execute_batch(&format!("CREATE TRIGGER lose_second_claim BEFORE UPDATE OF claim_token ON queued_work_batches WHEN OLD.batch_id = '{second}' BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            case.claim().await.is_none(),
            "{entry:?}: a partial claim must return no rows"
        );
        let owned: i64 = conn
            .query_row(
                "SELECT count(*) FROM queued_work_batches WHERE claim_token IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owned, 0, "{entry:?}: the first row must roll back");
        conn.execute_batch("DROP TRIGGER lose_second_claim")
            .unwrap();
        assert_eq!(
            case.claim().await.unwrap().batches.len(),
            2,
            "{entry:?}: both rows remain claimable"
        );
    }
}
