//! The session mail laws (ADR 0132 §3, §12; L3s, FIG-5196) on PostgreSQL.

use std::future::Future;
use std::pin::Pin;

/// Fails a producer transaction at the session actor's wake, the last
/// write before its commit.
struct WakeTrigger(sqlx::PgPool);

impl lash_conformance::WakeCut for WakeTrigger {
    fn arm(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            lash_postgres_store::testing::cut_session_wakes(&self.0, true)
                .await
                .expect("arm the wake cut");
        })
    }

    fn disarm(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            lash_postgres_store::testing::cut_session_wakes(&self.0, false)
                .await
                .expect("disarm the wake cut");
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_two_claimers_racing_admission_admit_one_run() {
    let Some((_database, storage)) = super::storage().await else {
        eprintln!("skipping Postgres session mail laws: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let (_attachments, stores) = super::pg_law_stores(&storage);
    lash_conformance::two_claimers_racing_admission_admit_one_run(stores, "postgres").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_producer_commits_its_row_and_its_wake_together() {
    let Some((_database, storage)) = super::storage().await else {
        eprintln!("skipping Postgres session mail laws: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let (_attachments, stores) = super::pg_law_stores(&storage);
    lash_conformance::a_producer_commits_its_row_and_its_wake_together(
        stores,
        "postgres",
        &WakeTrigger(storage.pool().clone()),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_producer_wakes_no_absent_or_deleted_session() {
    let Some((_database, storage)) = super::storage().await else {
        eprintln!("skipping Postgres session mail laws: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let (_attachments, stores) = super::pg_law_stores(&storage);
    lash_conformance::a_producer_wakes_no_absent_or_deleted_session(stores, "postgres").await;
}
