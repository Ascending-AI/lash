//! FIG-3927 N9's turn crash cells on PostgreSQL: a root's turns run inside
//! the Restate double's handlers over this test's PostgreSQL stores, so a
//! crash kills the handler execution where it stands and the recovery is
//! the double's redelivery of it, replaying its journal.
//!
//! Cell (a), the root admission crashed before its record, is the drive
//! admission law `a_root_admission_survives_a_worker_crash_without_widening`
//! in this suite's `root_control` module.

use std::sync::Arc;

use super::{double_law_backend, reset, storage};

/// The turn crash runner fixture: the store set under test, a maker of the
/// scenario's session store, the double's deployment host and its
/// handler-bound runner. `None` when no database is configured.
async fn crash_runner_fixture() -> Option<(
    impl Sized,
    Arc<dyn lash_core_execution::StoreSet>,
    impl Fn(&str) -> Arc<lash_postgres_store::PostgresStore> + Send + Sync + 'static,
    Arc<dyn lash_core_execution::EffectHost>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
)> {
    let (lock, storage) = storage().await?;
    reset(storage.pool()).await;
    let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
    let sessions = storage.clone();
    let make = move |scenario: &str| {
        Arc::new(sessions.session_store(format!("trace-derived-real-turn:{scenario}")))
    };
    Some((
        (lock, storage, attachments, double),
        stores,
        make,
        host,
        runner,
    ))
}

// Cells (b) and (c): a final commit whose reply was lost replays its receipt
// and settles nothing twice, and a checkpoint admission crashed before its
// record redelivers the rows it bound.
lash_conformance::turn_crash_admission_cells_tests!({
    let Some(fixture) = crash_runner_fixture().await else {
        eprintln!("skipping Postgres turn crash cells: database is not configured");
        return;
    };
    fixture
});
