//! Runs the handler-level tool-child invocation laws against the SQLite tier
//! (FIG-2266).
//!
//! The laws live in `lash-conformance` so every tier answers one set; this
//! file supplies the wiring — a host over one deployment's journal built with
//! the lease window the law asked for, the drain that host hands out over the
//! same journal, and a fresh process registry per scenario.
//!
//! Its own module rather than a case in the suite body: the recovery law
//! destroys a Tokio runtime on purpose, and a law that kills the runtime it is
//! running on cannot share a fixture with laws that do not.

use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{
    ToolChildDeferrableRouting, ToolChildLawFixture, ToolChildWorld, ToolChildWorldSpec,
};
use lash_core_execution::EffectHost;

use super::{Retained, SUBSTRATE, with_lease_timings};
use crate::deployment_fixture::{TestDeployment, system_clock};

/// A world over one deployment's journal.
///
/// The host is opened *inside* the returned future rather than cloned from an
/// outer one, because the recovery law calls this factory from the runtime it
/// is about to destroy: the SQLite connection must belong to that runtime so
/// it dies with it.
async fn world(deployment: TestDeployment, spec: ToolChildWorldSpec) -> ToolChildWorld {
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let host = deployment
        .reopen_with(
            with_lease_timings(
                lash_core_execution::facade_support::LeaseTimings::new(ttl, ttl / 3)
                    .expect("the law asks for a ttl at least three renew intervals wide"),
            ),
            system_clock(),
        )
        .await
        .effect_host();
    let drain = host.group_drain();
    ToolChildWorld {
        host: host as Arc<dyn EffectHost>,
        drain: Some(drain),
    }
}

/// The fixture both catalogues share: one deployment per invocation (the
/// macro evaluates the block per law), a world factory over its journal, and a
/// fresh deployment's process registry per scenario.
fn fixture() -> (
    (TestDeployment, Retained),
    &'static str,
    ToolChildLawFixture,
) {
    let deployment = TestDeployment::blocking(SUBSTRATE);
    let worlds = deployment.clone();
    let make_world: lash_conformance::ToolChildWorldFactory =
        Arc::new(move |spec: ToolChildWorldSpec| Box::pin(world(worlds.clone(), spec)));
    // Each scenario opens its own deployment, so a durable registry cannot
    // carry the previous scenario's rows.
    let retained = Retained::default();
    let registries = retained.clone();
    let make_registry: lash_conformance::ToolChildRegistryFactory = Arc::new(move || {
        let registries = registries.clone();
        Box::pin(async move {
            let deployment = TestDeployment::open(SUBSTRATE).await;
            registries.keep(&deployment);
            deployment.process_registry() as Arc<dyn lash_core_execution::ProcessRegistry>
        })
    });
    (
        (deployment, retained),
        "sqlite",
        ToolChildLawFixture {
            make_world,
            make_registry,
            deferrable_routing: ToolChildDeferrableRouting::Durable,
        },
    )
}

// The durable SQLite tier answers the tool-child invocation contract
// (FIG-2266).
lash_conformance::tool_child_invocation_tests!({ fixture() });

// The batch-group law answers on the same substrate (FIG-3397).
lash_conformance::tool_batch_group_tests!({ fixture() });
