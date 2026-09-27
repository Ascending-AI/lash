//! The build-generation drain laws (FIG-3799, FIG-3884) on the in-process
//! server double: the store set the endpoint's own handlers read, so the
//! queued-run count the law drives is the one a Restate deployment's drain
//! status would compose.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

lash_conformance::generation_drain_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let stores = harness.law_stores();
    (
        harness,
        lash_conformance::GenerationDrainLawFixture {
            stores,
            prefix: "restate-double".to_owned(),
        },
    )
});
