//! The cross-tier tool-batch parallelism law (FIG-3400) on the in-process
//! server double: the endpoint's own turn runner drives each scenario's turn
//! inside a `ConformanceTurnProbe` handler, where a direct batch's leaves run
//! as overlapping group-child invocations (FIG-3397).
//!
//! The relay-dispatched routes scenario does not reach this tier: a nested
//! batch's leaves run on the relay child's own invocation journal, which
//! Restate replays by position — serially (FIG-3671) — so the producer
//! declares it cannot reach the relay rather than deadlock the law.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

lash_conformance::tool_batch_parallelism_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let host = harness.endpoint_host();
    let runner = harness.turn_runner();
    let stores = harness.law_stores();
    let mut producer = lash_conformance::parallel_model_tool_calls_producer();
    // The relay entry's nested batch rides the relay child's invocation
    // journal, which Restate replays serially (FIG-3671): the direct entry —
    // one parallel model response dispatched as one effect group — is the
    // coverage this tier can carry.
    producer.reaches_relay = false;
    (
        harness,
        "restate-double",
        host,
        stores,
        vec![producer],
        runner,
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
        vec![lash_conformance::parallel_model_tool_calls_producer()],
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
        &lash_conformance::parallel_model_tool_calls_producer(),
        child.width,
        child.catalog,
    )
    .await;
}
