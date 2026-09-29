use std::sync::{Arc, Mutex};

use lash_core_execution::store::ConformanceDeployment;

use crate::SUBSTRATE;
use crate::backend_fixture::TestBackend;

async fn catalog() -> (TestBackend, Arc<dyn ConformanceDeployment>) {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await as Arc<dyn ConformanceDeployment>;
    (backend, store)
}

#[tokio::test]
async fn window_is_frame_bounded() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_window_is_frame_bounded(store).await;
}

#[tokio::test]
async fn pages_are_bounded_and_pinned() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_pages_are_bounded_and_pinned(store).await;
}

#[tokio::test]
async fn fork_respects_ceiling() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_fork_respects_ceiling(store).await;
}

#[tokio::test]
async fn window_rejects_corrupt_anchors() {
    let guards = Arc::new(Mutex::new(Vec::<TestBackend>::new()));
    lash_conformance::history_window_rejects_corrupt_anchors(|_| {
        let guards = Arc::clone(&guards);
        async move {
            let (backend, store) = catalog().await;
            guards
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(backend);
            store
        }
    })
    .await;
}

lash_conformance::turn_commit_outcome_tests!({
    let (backend, store) = catalog().await;
    (backend, store)
});
