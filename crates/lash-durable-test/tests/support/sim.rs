//! A simulated deployment's database and settings, on the virtual clock.
//!
//! In a SQLite store set, each store call runs to its answer before its caller goes on, as an
//! in-process database would. The simulation's one runtime thread then sees
//! every call finish in the order it issued them, and its clock never moves
//! while one is pending: a call left on the connection's own thread answers
//! after however many polls the host's load takes, and the virtual time a
//! write lands at, and so the run's shape, would turn on it.
//!
//! A VM worker runs off the runtime, so each worker call holds the
//! virtual clock until the worker answered ([`workers`]), and its deadlines
//! are off the clock: a cell's guest is bounded by its instruction and
//! memory budgets.
//!
//! Prompt sections render on the composer's own threads, so the clock a
//! simulated store set is opened on waits the process's renders out
//! ([`wait_out_renders`]).
//!
//! A session with nothing to do stays hot for [`IDLE_EVICT`] before it
//! releases, as in lash-sim's deployment: a run ends once its session is
//! idle, so under the default minute every cell of a matrix would poll
//! through a minute of virtual time.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::{
    Backend, BackendParts, DurableSettings, NoProjectionProviders, StoreSet,
};
use lash_durable_test::{OffClockWork, SimClock};
use lash_sqlite_store::{SqliteStoreSet, SqliteStoreSetOptions};

/// How long a session with nothing to do stays hot before it releases.
pub const IDLE_EVICT: Duration = Duration::from_secs(1);

/// The settings a simulated deployment runs on.
pub fn settings() -> DurableSettings {
    DurableSettings {
        idle_evict: IDLE_EVICT,
        ..DurableSettings::default()
    }
}

/// [`Backend::for_testing`] over `stores`, on the simulation's settings.
pub fn backend(stores: Arc<dyn StoreSet>) -> Backend {
    Backend::assemble(BackendParts {
        stores,
        settings: settings(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
        formats: Vec::new(),
    })
    .expect("a simulated backend assembles")
}

/// The dialect's worker service with its run deadlines off the clock: a
/// cell's guest is bounded by its instruction and memory budgets.
pub fn untimed_workers() -> lash::rlm::WorkerService {
    use lash::rlm::Dialect as _;
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    lash::rlm::WorkerService::new(config)
}

/// [`untimed_workers`] for a simulation on `clock`: each worker call holds
/// the clock while it is in flight.
pub fn workers(clock: &Arc<SimClock>) -> lash::rlm::WorkerService {
    let clock = Arc::clone(clock);
    untimed_workers().with_call_hold(Arc::new(move || Box::new(clock.hold())))
}

/// The process's prompt renders: they run on the composer's threads, off
/// the simulation's runtime.
struct PromptRenders;

impl OffClockWork for PromptRenders {
    fn busy(&self) -> bool {
        lash_core_execution::plugin::prompt::PromptRenderPool::renders_in_flight().0
    }

    fn ended(&self) -> usize {
        lash_core_execution::plugin::prompt::PromptRenderPool::renders_in_flight().1
    }
}

/// Hold `clock` while a prompt render of the process is in flight.
pub fn wait_out_renders(clock: &SimClock) {
    clock.wait_out(Arc::new(PromptRenders));
}

/// A fresh SQLite in-memory store set on `clock`.
pub async fn memory(clock: Arc<SimClock>) -> SqliteStoreSet {
    wait_out_renders(&clock);
    let options = SqliteStoreSetOptions {
        inline_calls: true,
        ..SqliteStoreSetOptions::memory()
    };
    SqliteStoreSet::memory_with_options_and_clock(options, clock)
        .await
        .expect("an in-memory store set opens")
}

/// The SQLite file store set at `path` on `clock`.
pub async fn file(path: impl AsRef<Path>, clock: Arc<SimClock>) -> SqliteStoreSet {
    wait_out_renders(&clock);
    let options = SqliteStoreSetOptions {
        inline_calls: true,
        ..SqliteStoreSetOptions::default()
    };
    SqliteStoreSet::open_with_options_and_clock(path, options, clock)
        .await
        .expect("a file store set opens")
}
