//! FIG-3547's segment re-drive law on the in-process server double: the
//! law's segments run in the endpoint's real `LashProcessWorkflow`, a crash
//! fails the execution retryably so the double delivers it again over its
//! journal, and a lost substrate is the invocation killed and purged, then
//! submitted afresh under its key.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

lash_conformance::segment_redrive_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let prefix: &'static str =
        Box::leak(format!("segment-redrive-{}", harness.run_nonce()).into_boxed_str());
    let stores = harness.law_stores();
    let runner = harness.turn_runner();
    (harness, prefix, stores, runner)
});
