//! Runs the backend-agnostic loser-drain suite against the PostgreSQL tier
//! (FIG-1536).
//!
//! The suite lives in `lash-core` so both SQL tiers answer one set of laws; this
//! file supplies the wiring — a host over the configured database, built with
//! the lease window a law asked for, and the drain that host hands out when it
//! is given executors.
//!
//! Its own test binary rather than a case in `conformance.rs`: a crash law
//! destroys the Tokio runtime its phase ran on, and every sqlx connection that
//! runtime opened dies with it. That is exactly the effect the law wants and
//! exactly what a shared pool must not be exposed to, so the phase builds its
//! own `PostgresStorage` inside the runtime it is about to kill.

use std::sync::Arc;
use std::time::Duration;

use lash_core::EffectHost;
use lash_core::testing::conformance::{DrainWorld, DrainWorldFactory, DrainWorldSpec};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};

mod support;

use support::{SharedDatabaseLock, database_url};

/// A world over the configured database.
///
/// Connects rather than clones a pool: the crash law's phase must own its
/// connections so that dropping its runtime drops them, leaving the journal rows
/// under leases nobody renews — the state a killed process leaves behind.
async fn world(database_url: String, spec: DrainWorldSpec) -> DrainWorld {
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("PostgreSQL effect-group drain host");
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let options = PostgresEffectReplayOptions {
        lease_timings: lash_core::facade_support::LeaseTimings::new(ttl, ttl / 3)
            .expect("the suite asks for a ttl at least three renew intervals wide"),
    };
    let host = PostgresEffectHost::with_options(&storage, options);
    // `None` is a law's request for a host with no resolver at all, not a
    // default for this file to fill in: the drain such a host hands out is what
    // one of the laws is about.
    if let Some(executors) = spec.executors {
        host.register_group_executors(executors)
            .expect("a freshly connected host has no resolver yet");
    }
    let drain = host.group_drain();
    DrainWorld {
        host: Arc::new(host) as Arc<dyn EffectHost>,
        drain,
    }
}

/// The durable PostgreSQL tier answers the loser-drain contract (FIG-1536).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_the_loser_drain_contract_when_configured() {
    let Some(url) = database_url() else {
        eprintln!(
            "skipping Postgres loser-drain conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let make: DrainWorldFactory = Arc::new(move |spec: DrainWorldSpec| {
        let url = url.clone();
        Box::pin(async move { world(url, spec).await })
    });
    lash_core::testing::conformance::effect_group_drain_conformance(make).await;
    drop(database_lock);
}
