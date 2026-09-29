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

/// The resumption half of the perf guard (FIG-4088): on an endpoint double
/// that replays at every await, the `INACTIVITY_TIMEOUT=0s` mode of the e2e
/// replay leg, a width-64 group's dispatch and opener resume about as often as
/// a width-8 group's, held to `scripts/perf_guard_budgets.json`. A dispatch
/// that suspended once per child, or an opener that suspended once per rank,
/// replayed its whole journal each time.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn tool_batch_resumptions_stay_bounded() {
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
    let producer = lash_conformance::parallel_model_tool_calls_producer(standard_factories());
    let mut measured = Vec::new();
    for width in [budget.small_width, budget.large_width] {
        measured.push(
            lash_conformance::measure_tool_batch_resumptions(
                "resumptions",
                harness.endpoint_host(),
                harness.law_stores(),
                harness.turn_runner(),
                &producer,
                width,
                budget.large_width,
                || resumption_counts(&server),
            )
            .await,
        );
    }
    lash_conformance::assert_tool_batch_resumptions_bounded(
        "restate-endpoint-double/always-replay/parallel-model-tool-calls",
        measured[0],
        measured[1],
        budget,
    );
}

/// Every group dispatch's and every turn's suspensions on `server`, once no
/// dispatch still runs: a dispatch holds its children's calls past the turn
/// that opened the group. The scenario's turn is the group's opener.
async fn resumption_counts(
    server: &lash_restate_test::RestateTestServer,
) -> lash_conformance::ToolBatchResumptionCounts {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let invocations = server.invocations();
        let dispatches = invocations
            .iter()
            .filter(|invocation| {
                invocation.target.contains("EffectGroupDispatch")
                    && invocation.target.ends_with("/run")
            })
            .collect::<Vec<_>>();
        if dispatches
            .iter()
            .all(|invocation| invocation.status == "completed")
        {
            let suspensions =
                |invocations: &mut dyn Iterator<Item = &lash_restate_test::InvocationView>| {
                    invocations
                        .map(|invocation| u64::from(invocation.suspensions))
                        .sum()
                };
            return lash_conformance::ToolBatchResumptionCounts {
                dispatch: suspensions(&mut dispatches.iter().copied()),
                opener: suspensions(
                    &mut invocations
                        .iter()
                        .filter(|invocation| invocation.target.contains("ConformanceTurnProbe")),
                ),
            };
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a group dispatch did not finish: {dispatches:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
