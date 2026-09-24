//! Runs the backend-agnostic loser-drain suite against the SQLite tier
//! (FIG-1536).
//!
//! The suite lives in `lash-core` so both SQL tiers answer one set of laws. All
//! this file supplies is the wiring the laws are about: a host over one
//! backend's journal, built with the lease window the law asked for,
//! registered with the resolver the law supplied, and the drain that host hands
//! out over the same journal and the same resolver.
//!
//! Its own module rather than a case in the group-contract file: the drain laws
//! destroy Tokio runtimes on purpose, and a law that kills the runtime it is
//! running on cannot share a fixture with laws that do not.

use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{DrainWorld, DrainWorldFactory, DrainWorldSpec};
use lash_core_execution::EffectHost;
use lash_sqlite_store::{SqliteBackendOptions, SqliteEffectReplayOptions};

use super::SUBSTRATE;
use crate::backend_fixture::{TestBackend, system_clock};

/// A world over one backend's journal.
///
/// The host is opened *inside* the returned future rather than cloned from an
/// outer one, because a crash law calls this factory from the runtime it is
/// about to destroy: the SQLite connection must belong to that runtime so it
/// dies with it.
async fn world(backend: TestBackend, spec: DrainWorldSpec) -> DrainWorld {
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let effect_replay = SqliteEffectReplayOptions {
        lease_timings: lash_core_execution::facade_support::LeaseTimings::new(ttl, ttl / 3)
            .expect("the suite asks for a ttl at least three renew intervals wide"),
        drain_budget: spec
            .drain_budget
            .map(lash_core_execution::EffectGroupDrainBudget::new)
            .unwrap_or_default(),
    };
    let host = backend
        .reopen_with(
            move |options| SqliteBackendOptions {
                effect_replay,
                ..options
            },
            system_clock(),
        )
        .await
        .effect_host();
    // `None` is a law's request for a host with no resolver at all, not a
    // default for this file to fill in: the drain such a host hands out is what
    // one of the laws is about.
    if let Some(resolver) = spec.executors {
        host.register_group_executors(resolver)
            .expect("a freshly opened host has no resolver yet");
    }
    let drain = host.group_drain();
    let journal_faults = host.effect_journal_faults();
    DrainWorld {
        host: host as Arc<dyn EffectHost>,
        drain,
        journal_faults,
    }
}

// The durable SQLite tier answers the loser-drain contract (FIG-1536).
lash_conformance::store_effect_group_drain_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let worlds = backend.clone();
    let make: DrainWorldFactory =
        Arc::new(move |spec: DrainWorldSpec| Box::pin(world(worlds.clone(), spec)));
    (backend, make)
});

// The durable SQLite tier answers the §7 durable-closing contract (FIG-3410) —
// the same world factory, the closing seam beside the drain on the same host.
lash_conformance::store_effect_group_closing_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let worlds = backend.clone();
    let make: DrainWorldFactory =
        Arc::new(move |spec: DrainWorldSpec| Box::pin(world(worlds.clone(), spec)));
    (backend, make)
});
