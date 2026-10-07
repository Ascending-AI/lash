//! The session mail laws (ADR 0132 §3, §12; L3s, FIG-5196) on SQLite.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

/// Fails a producer transaction at the session actor's wake, the last
/// write before its commit.
struct WakeTrigger(TestBackend);

impl lash_conformance::WakeCut for WakeTrigger {
    fn arm(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            lash_sqlite_store::testing::cut_session_wakes(&self.0.raw(), true)
                .expect("arm the wake cut");
        })
    }

    fn disarm(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            lash_sqlite_store::testing::cut_session_wakes(&self.0.raw(), false)
                .expect("disarm the wake cut");
        })
    }
}

fn stores(backend: &TestBackend) -> Arc<dyn lash_core_execution::StoreSet> {
    Arc::new((**backend).clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_claimers_racing_admission_admit_one_run() {
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_conformance::two_claimers_racing_admission_admit_one_run(stores(&backend), "sqlite").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_commits_its_row_and_its_wake_together() {
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_conformance::a_producer_commits_its_row_and_its_wake_together(
        stores(&backend),
        "sqlite",
        &WakeTrigger(backend.clone()),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_wakes_no_absent_or_deleted_session() {
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_conformance::a_producer_wakes_no_absent_or_deleted_session(stores(&backend), "sqlite")
        .await;
}
