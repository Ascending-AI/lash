//! Runs the handler-level tool-child invocation laws against the PostgreSQL
//! tier (FIG-2266).
//!
//! The laws live in `lash-conformance` so every tier answers one set; this
//! file supplies the wiring — a host over the configured database, built with
//! the lease window a law asked for, and the drain that host hands out.
//!
//! Included as a `#[path]` module of `main.rs` rather than its own binary so
//! it can share `crate::support`: the recovery law destroys a Tokio runtime on
//! purpose, and every sqlx connection that runtime opened dies with it, so the
//! phase builds its own `PostgresStorage` inside the runtime it is about to
//! kill.

use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{
    ToolChildDeferrableRouting, ToolChildLawFixture, ToolChildWorld, ToolChildWorldSpec,
};
use lash_core::EffectHost;
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};

use crate::support::{SharedDatabaseLock, database_url, reset};

/// A world over the configured database.
///
/// Connects rather than clones a pool: the recovery law's phase must own its
/// connections so that dropping its runtime drops them, leaving the journal
/// rows under claims nobody renews — the state a killed worker leaves behind.
async fn world(database_url: String, spec: ToolChildWorldSpec) -> ToolChildWorld {
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("PostgreSQL tool-child host storage");
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let options = PostgresEffectReplayOptions {
        lease_timings: lash_core::facade_support::LeaseTimings::new(ttl, ttl / 3)
            .expect("the law asks for a ttl at least three renew intervals wide"),
        drain_budget: Default::default(),
    };
    let host = PostgresEffectHost::with_options(&storage, options);
    let drain = host.group_drain();
    ToolChildWorld {
        host: Arc::new(host) as Arc<dyn EffectHost>,
        drain: Some(drain),
    }
}

// The durable PostgreSQL tier answers the tool-child invocation contract
// (FIG-2266).
lash_conformance::tool_child_invocation_tests!({
    let Some(url) = database_url() else {
        eprintln!(
            "skipping Postgres tool-child invocation conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    // The laws' scenario ids are deterministic and this database outlives the
    // test process: without a reset a rerun replays the previous run's
    // journaled rows and the bodies the laws watch for never re-execute.
    reset(
        PostgresStorage::connect(&url)
            .await
            .expect("PostgreSQL tool-child reset storage")
            .pool(),
    )
    .await;
    let world_url = url.clone();
    let make_world: lash_conformance::ToolChildWorldFactory =
        Arc::new(move |spec: ToolChildWorldSpec| {
            let url = world_url.clone();
            Box::pin(async move { world(url, spec).await })
        });
    // Process ids are session-scoped and every scenario's session is unique,
    // so one registry over the shared database cannot confuse two scenarios;
    // connecting per call keeps the handle inside the caller's runtime.
    let registry_url = url.clone();
    let make_registry: lash_conformance::ToolChildRegistryFactory = Arc::new(move || {
        let url = registry_url.clone();
        Box::pin(async move {
            let storage = PostgresStorage::connect(&url)
                .await
                .expect("PostgreSQL tool-child process registry storage");
            Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessRegistry>
        })
    });
    (
        database_lock,
        "postgres",
        ToolChildLawFixture {
            make_world,
            make_registry,
            deferrable_routing: ToolChildDeferrableRouting::Durable,
        },
    )
});

// The batch-group law answers on the same substrate (FIG-3397).
lash_conformance::tool_batch_group_tests!({
    let Some(url) = database_url() else {
        eprintln!(
            "skipping Postgres tool-batch-group conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    reset(
        PostgresStorage::connect(&url)
            .await
            .expect("PostgreSQL tool-batch-group reset storage")
            .pool(),
    )
    .await;
    let world_url = url.clone();
    let make_world: lash_conformance::ToolChildWorldFactory =
        Arc::new(move |spec: ToolChildWorldSpec| {
            let url = world_url.clone();
            Box::pin(async move { world(url, spec).await })
        });
    let registry_url = url.clone();
    let make_registry: lash_conformance::ToolChildRegistryFactory = Arc::new(move || {
        let url = registry_url.clone();
        Box::pin(async move {
            let storage = PostgresStorage::connect(&url)
                .await
                .expect("PostgreSQL tool-batch-group process registry storage");
            Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessRegistry>
        })
    });
    (
        database_lock,
        "postgres",
        ToolChildLawFixture {
            make_world,
            make_registry,
            deferrable_routing: ToolChildDeferrableRouting::Durable,
        },
    )
});
