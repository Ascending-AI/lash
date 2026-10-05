//! The live-fault park laws (FIG-4651) on the in-process server double: a
//! live fault installing a recorded tool surface, or at a checkpoint's store
//! admission, fails the attempt unrecorded; the double retries the invocation
//! and pauses it once its retries run out. One registration per store tier
//! the law's runtime admits through. The `live` module registers the same
//! laws on a live server.

use std::sync::Arc;

use super::conformance_harness::{HarnessServer, LiveConformanceHarness};
use super::harness_store_tiers::HarnessStoreTier;

/// Ends the turns a law deliberately left rested.
type Release =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

type Fixture = (
    Arc<LiveConformanceHarness>,
    String,
    Arc<dyn lash_core::EffectHost>,
    Arc<dyn lash_core::StoreSet>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
    Release,
);

fn fixture(harness: LiveConformanceHarness, tier: &str) -> Fixture {
    let harness = Arc::new(harness);
    let host = harness.endpoint_host();
    let runner = harness.turn_runner();
    let prefix = format!("restate-fault-park-{tier}-{}", harness.run_nonce());
    let stores = harness.law_stores();
    // A live server's census rejects unfinished invocations; the double
    // keeps none past its own lifetime.
    let live = tier == "live";
    let teardown = Arc::clone(&harness);
    let release: Release = Box::new(move || {
        Box::pin(async move {
            if live {
                teardown
                    .kill_open("the live-fault park law deliberately leaves its turn paused")
                    .await;
            }
        })
    });
    (harness, prefix, host, stores, runner, release)
}

async fn double_fixture(tier: HarnessStoreTier, name: &str) -> Fixture {
    fixture(
        LiveConformanceHarness::start_for_tools_over(HarnessServer::in_process(), tier).await,
        name,
    )
}

mod over_sqlite {
    use super::*;

    lash_conformance::live_fault_park_tests!({
        double_fixture(HarnessStoreTier::SqliteMemory, "sqlite").await
    });
}

mod over_postgres {
    use super::*;

    lash_conformance::live_fault_park_tests!(
        #[ignore = "requires isolated PostgreSQL; run through the run-conformance suite with pg16"]
        {
            double_fixture(HarnessStoreTier::Postgres, "postgres").await
        }
    );
}

/// The laws on a live Restate server, which pauses the invocation once its
/// retries run out.
mod live {
    use super::*;

    lash_conformance::live_fault_park_tests!(
        #[ignore = "requires an isolated Restate server; run by the run-conformance suite"]
        {
            fixture(LiveConformanceHarness::start_for_tools().await, "live")
        }
    );
}
