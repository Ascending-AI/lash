//! The native tier's registration of the handler-level tool-child invocation
//! laws (FIG-2266).
//!
//! The in-memory host keeps no journal across a process boundary, so the world
//! factory returns the same host for every call and reports no drain: the
//! recovery law reads that and asserts the routing gate at first open, which
//! is the only place this tier can speak to it.

use std::sync::Arc;

use crate::*;

crate::tool_child_invocation_tests!({
    // The deferred route parks on a completion key, and the native host issues
    // one only for an embedding that accepts that such a key dies with the
    // process — which a single-process conformance run is.
    let host: Arc<dyn crate::EffectHost> = Arc::new(
        crate::NativeEffectHost::with_native_controller(Arc::new(
            NativeRuntimeEffectController::default(),
        ))
        .allow_process_lifetime_completion_keys(),
    );
    (
        (),
        "native",
        crate::ToolChildLawFixture {
            make_world: Arc::new(move |_spec| {
                let host = Arc::clone(&host);
                Box::pin(async move { crate::ToolChildWorld { host, drain: None } })
            }),
            make_processes: Arc::new(|| {
                Box::pin(async {
                    crate::ToolChildProcesses {
                        registry: Arc::new(crate::TestLocalProcessRegistry::default())
                            as Arc<dyn crate::ProcessRegistry>,
                        process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new())
                            as Arc<dyn crate::ProcessExecutionEnvStore>,
                    }
                })
            }),
            deferrable_routing: crate::ToolChildDeferrableRouting::ProcessLifetime,
        },
    )
});

// The batch-group law answers on the same substrate.
crate::tool_batch_group_tests!({
    let host: Arc<dyn crate::EffectHost> = Arc::new(
        crate::NativeEffectHost::with_native_controller(Arc::new(
            NativeRuntimeEffectController::default(),
        ))
        .allow_process_lifetime_completion_keys(),
    );
    (
        (),
        "native",
        crate::ToolChildLawFixture {
            make_world: Arc::new(move |_spec| {
                let host = Arc::clone(&host);
                Box::pin(async move { crate::ToolChildWorld { host, drain: None } })
            }),
            make_processes: Arc::new(|| {
                Box::pin(async {
                    crate::ToolChildProcesses {
                        registry: Arc::new(crate::TestLocalProcessRegistry::default())
                            as Arc<dyn crate::ProcessRegistry>,
                        process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new())
                            as Arc<dyn crate::ProcessExecutionEnvStore>,
                    }
                })
            }),
            deferrable_routing: crate::ToolChildDeferrableRouting::ProcessLifetime,
        },
    )
});
