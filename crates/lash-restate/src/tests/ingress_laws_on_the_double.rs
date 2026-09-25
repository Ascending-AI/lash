//! The one session ingress's runtime laws and cancel by author (FIG-3600 S8,
//! FIG-3543) on the in-process server double, through the endpoint's real
//! handlers.

lash_conformance::ingress_runtime_tests!({
    let harness =
        super::effect_group_conformance::LiveConformanceHarness::start_for_tool_children_on(
            super::effect_group_conformance::HarnessServer::in_process(),
        )
        .await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-ingress-runtime-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

lash_conformance::cancel_by_author_tests!({
    let harness =
        super::effect_group_conformance::LiveConformanceHarness::start_for_tool_children_on(
            super::effect_group_conformance::HarnessServer::in_process(),
        )
        .await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-cancel-by-author-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});
