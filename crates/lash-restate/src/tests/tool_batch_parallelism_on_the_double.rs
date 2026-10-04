//! The barrier laws (FIG-3400, ADR 0116 §7.1) and the `batch` sugar laws
//! (§7.2) on the in-process server double:
//! the endpoint's own turn runner executes each scenario's turn inside a
//! `ConformanceTurnProbe` handler, where native calls and expanded `batch`
//! members run as calls of the logical opener's Run.

use super::conformance_harness::{HarnessServer, LiveConformanceHarness};

/// The standard protocol, which expands `batch` into the Run's tool round.
pub(super) fn standard_factories()
-> Vec<std::sync::Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![std::sync::Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    )]
}

lash_conformance::tool_batch_parallelism_tests!({
    let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
    let host = harness.endpoint_host();
    let runner = harness.turn_runner();
    let stores = harness.law_stores();
    (
        harness,
        "restate-double",
        host,
        stores,
        vec![
            lash_conformance::batch_sugar_producer(standard_factories()),
            lash_conformance::batch_wrappers_beside_native_calls_producer(standard_factories()),
            lash_conformance::parallel_model_tool_calls_producer(standard_factories()),
        ],
        runner,
    )
});

/// The standard protocol with `batch` withheld.
pub(super) fn withheld_factories()
-> Vec<std::sync::Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![std::sync::Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::with_config(
            lash_protocol_standard::StandardProtocolConfig::default()
                .batch(lash_protocol_standard::BatchSugar::Disabled),
        ),
    )]
}

// The `batch` sugar laws (ADR 0116 §7.2) on the same tier.
lash_conformance::batch_sugar_tests!({
    let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
    let host = harness.endpoint_host();
    let runner = harness.turn_runner();
    let stores = harness.law_stores();
    (
        harness,
        "restate-double",
        host,
        stores,
        runner,
        lash_conformance::BatchSugarFactories {
            enabled: standard_factories(),
            disabled: withheld_factories(),
        },
    )
});
