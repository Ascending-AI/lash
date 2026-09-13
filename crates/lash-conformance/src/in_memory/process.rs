use crate::*;
use std::sync::Arc;

crate::process_registry_tests!({
    ((), |_: &str| {
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn crate::ConformanceProcessRegistry>
    })
});

// No process_registry_reopenable_tests!: an in-memory registry has no cold-reopen boundary.
// No process_prune_reclaim_tests!: the registry has no coupled durable session/blob factory.

crate::process_change_horizon_tests!({
    (
        (),
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>,
    )
});

crate::process_projection_repair_tests!({
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let registry_for_corruption = Arc::clone(&registry);
    (
        (),
        registry as Arc<dyn ProcessRegistry>,
        move |stale| async move {
            registry_for_corruption
                .replace_process_projection_for_testing(stale)
                .await;
        },
    )
});
