//! The `@foreign` arm of `effect_host_await_event_tests` — FIG-3429's
//! completion-routing pairwise refusal — on the in-process server double:
//! keys minted under this endpoint's authority resolve and peek through its
//! durable-wait services, while a host under a foreign authority refuses them
//! before ingress. The witnessed arm already runs here through
//! `effect_host_await_event_witness_tests`.

use std::sync::Arc;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use super::*;

lash_conformance::effect_host_await_event_tests!(@foreign {
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let server = harness
        .server_double()
        .expect("the completion-routing fixture runs on the in-process double");
    let make = harness.effect_host_factory();
    // A host under another authority is another registry: keys this
    // endpoint's authority minted fail its issuer check.
    let make_foreign = move || {
        Arc::new(RestateEffectHost::new(
            RestateConnection::with_transport(
                server.ingress_url().to_string(),
                server.transport(),
            ),
            RestateAuthorityId::new("lash-restate-foreign")
                .expect("valid foreign authority id"),
        )) as Arc<dyn EffectHost>
    };
    (harness, make, (), make_foreign)
}; [(completion_routing_pairwise_refusal, "completion-routing-pairwise")]);
