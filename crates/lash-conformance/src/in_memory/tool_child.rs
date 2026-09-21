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
        crate::NativeEffectHost::new(
            Arc::new(NativeRuntimeEffectController::default()) as Arc<dyn RuntimeEffectController>
        )
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
            make_registry: Arc::new(|| {
                Box::pin(async {
                    Arc::new(crate::TestLocalProcessRegistry::default())
                        as Arc<dyn crate::ProcessRegistry>
                })
            }),
            completion_routing: crate::runtime::effect::ToolChildCompletionRouting::ProcessLifetime,
        },
    )
});
