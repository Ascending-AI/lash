use lash_core_execution::{RuntimeStore, SessionCatalogStore as _};
use std::sync::Arc;
#[path = "../../../lash-core/tests/support/queued_admission_atomicity.rs"]
mod law;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_partial_checkpoint_admission_rolls_back() {
    let Some((_lock, storage)) = super::storage().await else {
        return;
    };
    super::reset(storage.pool()).await;
    storage
        .store()
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(
                &lash_sansio::SessionId::from("root"),
            ),
        )
        .await
        .expect("admit queued-admission run");
    let case = law::prepare(Arc::new(storage.store()) as Arc<dyn RuntimeStore>).await;
    sqlx::query("CREATE OR REPLACE FUNCTION lose_second_bind() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$").execute(storage.pool()).await.unwrap();
    let second = case.ids[1].replace('\'', "''");
    sqlx::query(&format!("CREATE TRIGGER lose_second_bind BEFORE UPDATE OF admitted_run ON lash_queued_work_batches FOR EACH ROW WHEN (OLD.batch_id = '{second}') EXECUTE FUNCTION lose_second_bind()")).execute(storage.pool()).await.unwrap();
    let admitted = case.admit().await;
    let bound: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lash_queued_work_batches WHERE admitted_run IS NOT NULL",
    )
    .fetch_one(storage.pool())
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER lose_second_bind ON lash_queued_work_batches")
        .execute(storage.pool())
        .await
        .unwrap();
    assert!(admitted.is_err(), "a partial admission is refused");
    assert_eq!(bound, 0, "the first row's bind must roll back");
    assert_eq!(
        case.admit().await.unwrap().len(),
        2,
        "both rows remain admissible"
    );
    sqlx::query("DROP FUNCTION lose_second_bind()")
        .execute(storage.pool())
        .await
        .unwrap();
}
