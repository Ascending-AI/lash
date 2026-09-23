//! The native tier's registration of the cross-tier tool-batch parallelism
//! law (FIG-3400).

use std::sync::Arc;

crate::tool_batch_parallelism_tests!({
    // The deferred route parks on a completion key, and the native host refuses
    // to issue one until the embedding accepts that such a key dies with the
    // process. A single-process conformance run is exactly that embedding, and
    // saying so here is what lets the native tier answer the same law as the
    // durable tiers instead of a narrower one.
    let host: Arc<dyn crate::EffectHost> =
        Arc::new(crate::NativeEffectHost::default().allow_process_lifetime_completion_keys());
    (
        (),
        "native",
        Arc::clone(&host),
        // Every producer the native tier reaches from this crate. The RLM
        // `Promise.all` bridge and the Lashlang process bridge live above
        // lash-conformance in the dependency graph and register the same law
        // from their own crates.
        vec![crate::parallel_model_tool_calls_producer()],
        crate::HostTurnRunner::shared(host),
    )
});
