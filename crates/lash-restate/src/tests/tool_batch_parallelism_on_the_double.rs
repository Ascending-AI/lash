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

/// The perf guard (FIG-4068): a width-64 batch of native parallel calls on
/// the endpoint double costs linear time and peak RSS in its width, held to
/// `scripts/perf_guard_budgets.json`.
#[test]
fn tool_batch_scales_linearly() {
    lash_conformance::assert_tool_batch_scales_linearly(
        "restate-endpoint-double/parallel-model-tool-calls",
        module_path!(),
        "tool_batch_scaling_child",
        lash_conformance::ToolBatchScalingBudget::from_perf_guard_budgets(include_str!(
            "../../../../scripts/perf_guard_budgets.json"
        )),
    );
}

/// One width of [`tool_batch_scales_linearly`], on a fresh endpoint double
/// in a process of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a width child of tool_batch_scales_linearly: only its re-execution runs it"]
async fn tool_batch_scaling_child() {
    let child = lash_conformance::tool_batch_scaling_child()
        .expect("the parent names the width to measure");
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    lash_conformance::run_tool_batch_scaling_child(
        "scaling",
        harness.endpoint_host(),
        harness.law_stores(),
        harness.turn_runner(),
        &lash_conformance::parallel_model_tool_calls_producer(standard_factories()),
        child.width,
        child.catalog,
    )
    .await;
}

/// The perf guard (FIG-4068) for `batch`: a width-64 `batch` on the endpoint
/// double costs linear time and peak RSS in its members, held to
/// `scripts/perf_guard_budgets.json`.
#[test]
fn batch_scales_linearly() {
    lash_conformance::assert_tool_batch_scales_linearly(
        "restate-endpoint-double/batch",
        module_path!(),
        "batch_scaling_child",
        lash_conformance::ToolBatchScalingBudget::from_perf_guard_budgets(include_str!(
            "../../../../scripts/perf_guard_budgets.json"
        )),
    );
}

/// One width of [`batch_scales_linearly`], on a fresh endpoint double in a
/// process of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a width child of batch_scales_linearly: only its re-execution runs it"]
async fn batch_scaling_child() {
    let child = lash_conformance::tool_batch_scaling_child()
        .expect("the parent names the width to measure");
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    lash_conformance::run_tool_batch_scaling_child(
        "batch-scaling",
        harness.endpoint_host(),
        harness.law_stores(),
        harness.turn_runner(),
        &lash_conformance::batch_sugar_producer(standard_factories()),
        child.width,
        child.catalog,
    )
    .await;
}
