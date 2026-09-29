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
async fn inflated_fork_ceiling_cannot_expose_post_fork_source_nodes() {
    let (_backend, store) = catalog().await;
    lash_conformance::inflated_fork_ceiling_cannot_expose_post_fork_source_nodes(store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_selection_and_confirmation_share_one_snapshot() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_selection_and_confirmation_share_one_snapshot(store).await;
}

#[tokio::test]
async fn graph_generation_overflow_rolls_back_every_write() {
    let (_backend, store) = catalog().await;
    lash_conformance::graph_generation_overflow_rolls_back_every_write(store).await;
}

#[tokio::test]
async fn a_later_frame_open_cannot_rescue_earlier_root_nodes() {
    let (_backend, store) = catalog().await;
    lash_conformance::a_later_frame_open_cannot_rescue_earlier_root_nodes(store).await;
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
