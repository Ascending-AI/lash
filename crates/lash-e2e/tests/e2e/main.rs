//! The real-host E2E cases of `scripts/lash-e2e-manifest.json`.
//!
//! Each manifest permutation is one test function named
//! `<test>[_postgresql][_resume]`. A case boots real host processes, each
//! one lash node over the case's store, and certifies what they did from
//! their HTTP answers, their ledgers and the store. `scripts/e2e-gate.py`
//! builds the hosts, supplies the store and runs one case at a time; the
//! cases are ignored by an ordinary run, which has no hosts to boot.

use std::time::Duration;

/// The longest any case may run, kill-and-resume legs included.
const BUDGET: Duration = Duration::from_secs(150);

fn block_on(future: impl std::future::Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(future)
}

/// One test function per manifest permutation of a scenario.
macro_rules! case {
    ($name:ident, $store:ident, $leg:ident, $run:expr) => {
        #[test]
        #[ignore = "a real-host case: scripts/e2e-gate.py boots its hosts"]
        fn $name() -> anyhow::Result<()> {
            crate::block_on(lash_e2e::Case::run(
                stringify!($name),
                lash_e2e::Store::$store,
                lash_e2e::Leg::$leg,
                crate::BUDGET,
                async |case| $run(case).await,
            ))
        }
    };
}

mod browser;
mod cancel;
mod feeds;
mod fleet;
mod handover;
mod intents;
mod mcp;
mod operations;
mod provider;
mod retries;
mod state;
mod support;
mod telemetry;
mod tools;
mod waits;
mod workbench;
