use lash_core::{QueuedWorkStore, RuntimePersistence, SessionExecutionLeaseStore, StoreError};
use std::sync::Arc;
#[path = "../../../lash-core/tests/support/queued_claim_atomicity.rs"]
mod law;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_queued_work_partial_claim_rolls_back_through_all_entry_points() {
    let Some((_lock, storage)) = super::storage().await else {
        return;
    };
    for entry in law::ENTRIES {
        super::reset(&storage).await;
        let case = law::prepare(
            Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
            entry,
        )
        .await;
        sqlx::query("CREATE OR REPLACE FUNCTION lose_second_claim() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$").execute(storage.pool()).await.unwrap();
        let second = case.ids[1].replace('\'', "''");
        sqlx::query(&format!("CREATE TRIGGER lose_second_claim BEFORE UPDATE OF claim_token ON lash_queued_work_batches FOR EACH ROW WHEN (OLD.batch_id = '{second}') EXECUTE FUNCTION lose_second_claim()")).execute(storage.pool()).await.unwrap();
        let claim = case.claim().await;
        let owned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM lash_queued_work_batches WHERE claim_token IS NOT NULL",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        sqlx::query("DROP TRIGGER lose_second_claim ON lash_queued_work_batches")
            .execute(storage.pool())
            .await
            .unwrap();
        assert!(
            claim.is_none(),
            "{entry:?}: a partial claim must return no rows"
        );
        assert_eq!(owned, 0, "{entry:?}: the first row must roll back");
        assert_eq!(
            case.claim().await.unwrap().batches.len(),
            2,
            "{entry:?}: both rows remain claimable"
        );
    }
    sqlx::query("DROP FUNCTION lose_second_claim()")
        .execute(storage.pool())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_negative_and_exhausted_queued_work_fences_are_typed_when_configured() {
    let Some((_database_lock, storage)) = super::storage().await else {
        eprintln!("skipping Postgres fence corruption test: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    super::reset(&storage).await;
    let session_id = "postgres-fence-corrupt";
    let store = storage.session_store(session_id);
    let owner = lash_core::LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
    let lease = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "postgres-conformance-executor",
            &lash_core::LeaseClaimNonce::new(),
            120_000,
        )
        .await
        .expect("claim session lease")
        .acquired()
        .expect("session lease acquired");
    let batch = store
        .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
            session_id,
            lash_core::DeliveryPolicy::EarliestSafeBoundary,
            vec![lash_core::runtime::QueuedWorkPayload::session_command(
                lash_core::runtime::SessionCommand::RefreshToolCatalog {
                    reason: "fence test".to_string(),
                },
            )],
        ))
        .await
        .expect("enqueue queued work");

    sqlx::query("UPDATE lash_queued_work_batches SET claim_fencing_token = -1 WHERE batch_id = $1")
        .bind(&batch.batch_id)
        .execute(storage.pool())
        .await
        .expect("inject negative fence");
    let corrupt = store
        .list_queued_work(session_id)
        .await
        .expect_err("negative fence must refuse");
    assert!(matches!(
        corrupt,
        StoreError::StoredDataCorrupt {
            record_kind: "QueuedWorkBatch",
            ..
        }
    ));

    sqlx::query("UPDATE lash_queued_work_batches SET claim_fencing_token = $1 WHERE batch_id = $2")
        .bind(i64::MAX)
        .bind(&batch.batch_id)
        .execute(storage.pool())
        .await
        .expect("seed exhausted fence");
    let exhausted = store
        .claim_leading_ready_session_command(session_id, &lease.authority(), &owner)
        .await
        .expect_err("exhausted SQL fence must refuse");
    assert!(matches!(
        exhausted,
        StoreError::MonotonicCounterOverflow {
            counter: "queued_work_claim_fencing_token",
            current,
        } if current == i64::MAX as u64
    ));
}
