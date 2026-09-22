//! Runs the handler-level tool-child invocation laws against the SQLite tier
//! (FIG-2266).
//!
//! The laws live in `lash-conformance` so every tier answers one set; this
//! file supplies the wiring — a host over a fixed database file built with the
//! lease window the law asked for, the drain that host hands out over the same
//! journal, and a fresh process registry per scenario.
//!
//! Its own integration test rather than a case in `conformance.rs`: the
//! recovery law destroys a Tokio runtime on purpose, and a law that kills the
//! runtime it is running on cannot share a test binary's fixture with laws
//! that do not.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_conformance::{
    ToolChildDeferrableRouting, ToolChildLawFixture, ToolChildWorld, ToolChildWorldSpec,
};
use lash_core::EffectHost;
use lash_sqlite_store::{SqliteEffectHost, SqliteEffectReplayOptions, SqliteProcessRegistry};

/// A world over one database file.
///
/// The host is opened *inside* the returned future rather than cloned from an
/// outer one, because the recovery law calls this factory from the runtime it
/// is about to destroy: the SQLite connection must belong to that runtime so
/// it dies with it.
async fn world(path: PathBuf, spec: ToolChildWorldSpec) -> ToolChildWorld {
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let options = SqliteEffectReplayOptions {
        lease_timings: lash_core::facade_support::LeaseTimings::new(ttl, ttl / 3)
            .expect("the law asks for a ttl at least three renew intervals wide"),
        drain_budget: Default::default(),
    };
    let host = SqliteEffectHost::open_with_options(&path, options)
        .await
        .expect("SQLite effect host");
    let drain = host.group_drain();
    ToolChildWorld {
        host: Arc::new(host) as Arc<dyn EffectHost>,
        drain: Some(drain),
    }
}

// The durable SQLite tier answers the tool-child invocation contract
// (FIG-2266).
lash_conformance::tool_child_invocation_tests!({
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tool-child-effects.db");
    let make_world: lash_conformance::ToolChildWorldFactory =
        Arc::new(move |spec: ToolChildWorldSpec| {
            let path = path.clone();
            Box::pin(async move { world(path, spec).await })
        });
    // Each scenario opens its own registry file, so a durable registry cannot
    // carry the previous scenario's rows.
    let registry_root = dir.path().to_path_buf();
    let registry_counter = Arc::new(AtomicUsize::new(0));
    let make_registry: lash_conformance::ToolChildRegistryFactory = Arc::new(move || {
        let ordinal = registry_counter.fetch_add(1, Ordering::SeqCst);
        let path = registry_root.join(format!("tool-child-processes-{ordinal}.db"));
        let sessions = registry_root.join(format!("tool-child-sessions-{ordinal}"));
        Box::pin(async move {
            Arc::new(
                SqliteProcessRegistry::open(&path, sessions)
                    .await
                    .expect("open the scenario's SQLite process registry"),
            ) as Arc<dyn lash_core::ProcessRegistry>
        })
    });
    (
        dir,
        "sqlite",
        ToolChildLawFixture {
            make_world,
            make_registry,
            deferrable_routing: ToolChildDeferrableRouting::Durable,
        },
    )
});
