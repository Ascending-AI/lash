//! Runs the backend-agnostic loser-drain suite against the SQLite tier
//! (FIG-1536).
//!
//! The suite lives in `lash-core` so both SQL tiers answer one set of laws. All
//! this file supplies is the wiring the laws are about: a host over a fixed
//! database file, built with the lease window the law asked for, registered with
//! the resolver the law supplied, and the drain that host hands out over the
//! same journal and the same resolver.
//!
//! Its own integration test rather than a case in the group-contract file: the
//! drain laws destroy Tokio runtimes on purpose, and a law that kills the
//! runtime it is running on cannot share a test binary's fixture with laws that
//! do not.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{DrainWorld, DrainWorldFactory, DrainWorldSpec};
use lash_core::EffectHost;
use lash_sqlite_store::{SqliteEffectHost, SqliteEffectReplayOptions};

/// A world over one database file.
///
/// The host is opened *inside* the returned future rather than cloned from an
/// outer one, because a crash law calls this factory from the runtime it is
/// about to destroy: the SQLite connection must belong to that runtime so it
/// dies with it.
async fn world(path: PathBuf, spec: DrainWorldSpec) -> DrainWorld {
    let ttl = Duration::from_millis(spec.lease_ttl_ms);
    let options = SqliteEffectReplayOptions {
        lease_timings: lash_core::facade_support::LeaseTimings::new(ttl, ttl / 3)
            .expect("the suite asks for a ttl at least three renew intervals wide"),
    };
    let host = SqliteEffectHost::open_with_options(&path, options)
        .await
        .expect("SQLite effect host");
    // `None` is a law's request for a host with no resolver at all, not a
    // default for this file to fill in: the drain such a host hands out is what
    // one of the laws is about.
    if let Some(resolver) = spec.executors {
        host.register_group_executors(resolver)
            .expect("a freshly opened host has no resolver yet");
    }
    let drain = host.group_drain();
    DrainWorld {
        host: Arc::new(host) as Arc<dyn EffectHost>,
        drain,
    }
}

// The durable SQLite tier answers the loser-drain contract (FIG-1536).
lash_conformance::store_effect_group_drain_tests!({
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("effect-group-drain.db");
    let make: DrainWorldFactory = Arc::new(move |spec: DrainWorldSpec| {
        let path = path.clone();
        Box::pin(async move { world(path, spec).await })
    });
    (dir, make)
});
