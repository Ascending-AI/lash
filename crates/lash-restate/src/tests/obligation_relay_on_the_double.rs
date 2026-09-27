//! The obligation relay and recovery leader lease laws (ADR 0109 §1) on the
//! in-process server double: the store set the endpoint's own handlers read,
//! so the ledgers and the lease the laws drive are the ones a Restate
//! deployment's relay and recovery tick would.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

lash_conformance::obligation_relay_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let stores = harness.law_stores();
    (
        harness,
        lash_conformance::ObligationLawFixture {
            stores,
            prefix: "restate-double".to_owned(),
        },
    )
});

lash_conformance::recovery_leader_tests!(|label| {
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let store = harness.law_stores().recovery_leader();
    (
        harness,
        lash_conformance::LeaseLawFixture {
            store,
            name: format!("recovery:{label}"),
        },
    )
});
