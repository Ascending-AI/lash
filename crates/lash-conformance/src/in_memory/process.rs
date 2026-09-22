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

// The in-memory tier's drain-end world: one in-memory session-store factory,
// one local process registry, and one shared native controller as the group
// substrate. `group_host` is a second `NativeEffectHost` over the *same*
// controller — a different host, not a different substrate — so L7's foreign
// `Pending` shape collapses to this tier's honest shape: a locally running
// obligation the drain's epilogue waits on (the law documents the collapse).
crate::drain_end_tests!({
    (
        (),
        "in-memory-drain-end",
        Arc::new(|_label| {
            Box::pin(async move {
                let store_factory = Arc::new(crate::InMemorySessionStoreFactory::new());
                let request = crate::testing::store_fixtures::session_store_request(
                    &SessionId::from(crate::SESSION_ID),
                    "drain-end-model",
                    crate::SessionRelation::Root,
                );
                let store = store_factory
                    .create_store(&request)
                    .await
                    .expect("create the in-memory drain-end session store");
                let controller = Arc::new(crate::NativeRuntimeEffectController::default());
                controller
                    .register_group_executors(
                        crate::RecordingExecutors::settling() as Arc<dyn crate::GroupExecutors>
                    )
                    .expect("a fresh controller has no resolver yet");
                let effect_host: Arc<dyn crate::EffectHost> = Arc::new(
                    crate::NativeEffectHost::with_native_controller(Arc::clone(&controller)),
                );
                let group_host: Option<Arc<dyn crate::EffectHost>> = Some(Arc::new(
                    crate::NativeEffectHost::with_native_controller(controller),
                ));
                crate::DrainEndWorld {
                    store,
                    registry: Arc::new(TestLocalProcessRegistry::default()),
                    session_factory: store_factory,
                    effect_host,
                    group_host,
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = crate::DrainEndWorld> + Send>>
        }) as crate::DrainEndWorldFactory,
    )
});
