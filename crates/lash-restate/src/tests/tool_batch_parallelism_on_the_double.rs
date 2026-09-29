//! The barrier laws (FIG-3400, ADR 0116 §7.1) and the `batch` sugar laws
//! (§7.2) on the in-process server double:
//! the endpoint's own turn runner drives each scenario's turn inside a
//! `ConformanceTurnProbe` handler, where every member of the step's tool group
//! — native calls and `batch` members alike — runs as an overlapping
//! group-child invocation (FIG-3397).

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

/// The standard protocol, which expands `batch` into the step's group.
fn standard_factories() -> Vec<std::sync::Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![std::sync::Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    )]
}

lash_conformance::tool_batch_parallelism_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
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
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
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

// FIG-4064 on the double: the turn's handler execution is killed while its
// batch holds a settled member and a held one, and Restate's redelivery of
// the same invocation must reuse the settled member's recorded completion.
lash_conformance::tool_batch_crash_redrive_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let host = harness.endpoint_host();
    let runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-batch-redrive-{}", harness.run_nonce()).into_boxed_str());
    (
        harness,
        prefix,
        host,
        stores,
        vec![lash_conformance::parallel_model_tool_calls_producer(
            standard_factories(),
        )],
        runner,
    )
});

/// The native-call scaling guard counts dispatch and opener resumptions.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn tool_batch_scales_linearly() {
    assert_batch_scales_linearly(
        "restate-endpoint-double/parallel-model-tool-calls",
        lash_conformance::parallel_model_tool_calls_producer(standard_factories()),
    )
    .await;
}

/// The `batch` scaling guard counts dispatch and opener resumptions.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn batch_scales_linearly() {
    assert_batch_scales_linearly(
        "restate-endpoint-double/batch",
        lash_conformance::batch_sugar_producer(standard_factories()),
    )
    .await;
}

async fn assert_batch_scales_linearly(label: &str, producer: lash_conformance::ToolBatchProducer) {
    let budget = lash_conformance::ToolBatchScalingBudget::from_perf_guard_budgets(include_str!(
        "../../../../scripts/perf_guard_budgets.json"
    ));
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay: true,
    })
    .await;
    let server = harness
        .server_double()
        .expect("the guard runs on the server double");
    let mut measured = Vec::new();
    for width in [budget.small_width, budget.large_width] {
        measured.push(
            lash_conformance::measure_tool_batch_resumptions(
                "scaling",
                harness.endpoint_host(),
                harness.law_stores(),
                harness.turn_runner(),
                &producer,
                width,
                budget.large_width,
                || async {
                    lash_restate_test::tool_batch_resumption_counts(&server)
                        .await
                        .into()
                },
            )
            .await,
        );
    }
    lash_conformance::assert_tool_batch_resumptions_bounded(
        label,
        measured[0],
        measured[1],
        budget,
    );
}
