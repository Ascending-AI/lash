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
