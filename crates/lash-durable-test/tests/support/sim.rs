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

/// The dialect's worker service with its deadlines off the clock: a cell's
/// guest is bounded by its instruction and memory budgets. The checkout
/// deadline and the no-response watchdog measure the host: on a loaded one
/// they fail a cell's setup retryably, which a crash law reads as a replay
/// no cut accounts for and every law pays for in retried attempts.
pub fn untimed_workers() -> lash::vm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    config.deadlines.checkout = OFF_THE_CLOCK;
    config.protocol.no_response_watchdog = OFF_THE_CLOCK;
    lash::vm::WorkerService::new(config)
}

/// [`untimed_workers`] for a simulation on `clock`: each worker call holds
/// the clock while it is in flight.
pub fn workers(clock: &Arc<SimClock>) -> lash::vm::WorkerService {
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
    let stores = SqliteStoreSet::memory_with_options_and_clock(options, clock)
        .await
        .expect("an in-memory store set opens");
    lash_core_execution::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    stores
}

/// The SQLite file store set at `path` on `clock`.
pub async fn file(path: impl AsRef<Path>, clock: Arc<SimClock>) -> SqliteStoreSet {
    wait_out_renders(&clock);
    let options = SqliteStoreSetOptions {
        inline_calls: true,
        ..SqliteStoreSetOptions::standard(lash_sqlite_store::SqliteSynchronous::Normal)
    };
    let stores = SqliteStoreSet::open_with_options_and_clock(path, options, clock)
        .await
        .expect("a file store set opens");
    lash_core_execution::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    stores
}

/// One visit of the artifact-cleanup relay over `backend` to every ended
/// execution that still holds one of `attachments`.
///
/// A relay that visited an execution's guard while its turn ran deferred the
/// row by its policy's longest backoff. The guard is made due now, as an end
/// fact's nudge makes it, and the clock stays where it is. Moving the clock
/// past the backoff instead wakes the deployment's own relay and then moves
/// on under its PostgreSQL round trips, which the clock waits out only
/// within a node step: its attempts outlive their budget one after another,
/// and its claim and its retry backoff keep the row from every pass made
/// here (FIG-5672).
pub async fn relay_ended_executions(
    backend: &Backend,
    clock: &Arc<SimClock>,
    attachments: &[&lash_core::AttachmentId],
) -> Result<(), String> {
    let referrers = backend.attachment_referrers();
    let ledger = backend.artifact_cleanup();
    let now_ms = lash_core::ClockWallTime::timestamp_ms(clock.as_ref());
    for attachment in attachments {
        let held = referrers
            .attachment_referrers(attachment)
            .await
            .map_err(|error| format!("read an attachment's referrers: {error}"))?;
        for referrer in held {
            if matches!(referrer, lash_core::ArtifactReferrer::Execution(_)) {
                ledger
                    .nudge(&referrer, now_ms)
                    .await
                    .map_err(|error| format!("make `{referrer}`'s cleanup due: {error}"))?;
            }
        }
    }
    let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
        backend,
        lash_core::ProcessEngineRegistry::default(),
    );
    lash_core::runtime::obligations::relay::relay_due(
        &relay,
        clock.as_ref(),
        std::num::NonZeroUsize::new(256).expect("a page"),
    )
    .await
    .map_err(|error| format!("the cleanup relay's due pass: {error}"))?;
    Ok(())
}
