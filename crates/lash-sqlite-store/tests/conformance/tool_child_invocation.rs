//! Runs the handler-level tool-child invocation laws against the SQLite tier
//! (FIG-2266).
//!
//! The laws live in `lash-conformance` so every tier answers one set; this
//! file supplies the wiring — a host over one backend's journal built with
//! the lease window the law asked for, the drain that host hands out over the
//! same journal, and a fresh store set per scenario.
//!
//! Its own module rather than a case in the suite body: the recovery law
//! destroys a Tokio runtime on purpose, and a law that kills the runtime it is
//! running on cannot share a fixture with laws that do not.

use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{ToolChildLawFixture, ToolChildWorld, ToolChildWorldSpec};
use lash_core_execution::{EffectHost, StoreSet};

use super::{Retained, SUBSTRATE, with_lease_timings};
use crate::backend_fixture::{TestBackend, system_clock};

/// A world over one backend's journal.
///
/// The host is opened *inside* the returned future rather than cloned from an
/// outer one, because the recovery law calls this factory from the runtime it
/// is about to destroy: the SQLite connection must belong to that runtime so
/// it dies with it.
async fn world(backend: TestBackend, spec: ToolChildWorldSpec) -> ToolChildWorld {
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let host = backend
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

/// The fixture both catalogues share: one backend per invocation (the
/// macro evaluates the block per law), a world factory over its journal, and a
/// fresh backend's store set per scenario.
fn fixture() -> ((TestBackend, Retained), &'static str, ToolChildLawFixture) {
    let backend = TestBackend::blocking(SUBSTRATE);
    let worlds = backend.clone();
    let make_world: lash_conformance::ToolChildWorldFactory =
        Arc::new(move |spec: ToolChildWorldSpec| Box::pin(world(worlds.clone(), spec)));
    // Each scenario opens its own backend, so a durable registry cannot
    // carry the previous scenario's rows.
    let retained = Retained::default();
    let registries = retained.clone();
    let make_processes: lash_conformance::ToolChildProcessesFactory = Arc::new(move || {
        let registries = registries.clone();
        Box::pin(async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            registries.keep(&backend);
            Arc::new(backend.stores().clone()) as Arc<dyn StoreSet>
        })
    });
    (
        (backend, retained),
        "sqlite",
        ToolChildLawFixture {
            make_world,
            make_processes,
        },
    )
}

// The durable SQLite tier answers the tool-child invocation contract
// (FIG-2266).
lash_conformance::tool_child_invocation_tests!({ fixture() });

// The batch-group law answers on the same substrate (FIG-3397).
lash_conformance::tool_batch_group_tests!({ fixture() });
