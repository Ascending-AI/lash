use crate::*;
use std::sync::Arc;
#[tokio::test]
async fn test_local_process_registry_satisfies_conformance() {
    crate::conformance::process_registry(|| {
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn crate::ConformanceProcessRegistry>
    })
    .await;
}

#[tokio::test]
async fn test_local_process_registry_pagination_satisfies_conformance() {
    crate::conformance::process_registry_pagination(Arc::new(TestLocalProcessRegistry::default()))
        .await;
}

#[tokio::test]
async fn test_local_change_feed_refuses_cursor_below_tombstone_compaction_horizon() {
    crate::conformance::process_change_cursor_below_tombstone_compaction_horizon_is_refused(
        Arc::new(TestLocalProcessRegistry::default()),
    )
    .await;
}

#[tokio::test]
async fn test_local_process_prune_scopes_to_the_retention_filter() {
    crate::conformance::process_prune_scoped_by_originator(Arc::new(
        TestLocalProcessRegistry::default(),
    ))
    .await;
}

#[tokio::test]
async fn in_memory_leased_completion_replay_repairs_projection() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let registry_for_corruption = Arc::clone(&registry);
    crate::conformance::leased_completion_replay_repairs_projection(
        registry as Arc<dyn ProcessRegistry>,
        move |stale| async move {
            registry_for_corruption
                .replace_process_projection_for_testing(stale)
                .await;
        },
    )
    .await;
}
