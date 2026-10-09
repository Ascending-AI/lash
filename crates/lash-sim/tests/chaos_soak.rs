//! The chaos soak: every crash-matrix workload at once on the production
//! durable runtime, both nodes serving, under seeded node kills, restarts,
//! pauses and partitions (and, on PostgreSQL, lock-timeout storms and
//! database restarts), checked against the matrix's invariants. The harness
//! is `lash_sim::chaos_soak`; a failed epoch replays from the seed its
//! evidence prints (`LASH_CHAOS_SOAK_SEED`).
//!
//! An epoch an open finding (`lash_sim::crash_matrix::findings::OPEN`)
//! explains is reported, not failed.
//!
//! `chaos_soak_smoke` is the short profile: two epochs of twelve steps on
//! SQLite memory. `chaos_soak_on_postgres` runs the same profile on
//! PostgreSQL, with the database faults applied, when the run names a
//! server. The long profile, `chaos_soak_long` and
//! `chaos_soak_long_on_postgres`, runs epochs of forty steps for an hour of
//! wall time; release certification selects it by hand, past NativeLink's
//! action limit:
//!
//! ```sh
//! kiln test //crates/lash-sim:chaos_soak__test --local-test-execution \
//!   --no-test-cache --test_timeout=4500 --test_arg=--ignored \
//!   --test_arg=--exact --test_arg=chaos_soak_long --test_arg=--nocapture
//! ```

use std::time::Duration;

use lash_sim::chaos_soak::{self, SoakConfig, SoakReport};
use lash_sim::crash_matrix::deployment::Dialect;

/// The smoke seed when `LASH_CHAOS_SOAK_SEED` is unset.
const SMOKE_SEED: u64 = 0x5184_0001;
/// Epochs of the short profile when `LASH_CHAOS_SOAK_EPOCHS` is unset.
const SMOKE_EPOCHS: usize = 2;
/// Fault steps per epoch when `LASH_CHAOS_SOAK_STEPS` is unset.
const SMOKE_STEPS: usize = 12;
/// The short profile's wall-time budget.
const SMOKE_CAP: Duration = Duration::from_secs(4 * 60);
/// The long profile's first seed when `LASH_CHAOS_SOAK_SEED` is unset.
const LONG_SEED: u64 = 0x5193_0001;
/// The long profile's epochs when `LASH_CHAOS_SOAK_EPOCHS` is unset: more
/// than its wall time admits, so the cap ends it.
const LONG_EPOCHS: usize = 1_000;
/// The long profile's fault steps per epoch when `LASH_CHAOS_SOAK_STEPS` is
/// unset.
const LONG_STEPS: usize = 40;
/// The long profile's wall-time budget.
const LONG_CAP: Duration = Duration::from_secs(60 * 60);

fn assert_green(report: &SoakReport) {
    assert!(!report.epochs.is_empty(), "the soak ran no epoch");
    for epoch in &report.epochs {
        eprintln!(
            "epoch {:#x}: {} steps, {} writes ({} committed), {} ms\n  {}",
            epoch.seed,
            epoch.steps.len(),
            epoch.writes,
            epoch.committed,
            epoch.end_ms,
            epoch.applied.join("; ")
        );
    }
    for epoch in report.failed() {
        if let Some(finding) = lash_sim::crash_matrix::findings::explaining_epoch(&epoch.violations)
        {
            eprintln!(
                "epoch {:#x} shows open finding {} ({}): {}",
                epoch.seed, finding.id, finding.owner, finding.summary
            );
        }
    }
    let failed = report.unexplained();
    assert!(
        failed.is_empty(),
        "{} of {} chaos-soak epoch(s) failed:\n{}",
        failed.len(),
        report.epochs.len(),
        failed
            .iter()
            .map(|epoch| epoch.evidence())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The short profile: node kills, restarts, pauses and partitions over
/// every workload, on SQLite memory.
#[tokio::test]
async fn chaos_soak_smoke() {
    let config = SoakConfig::from_env(
        SMOKE_SEED,
        SMOKE_EPOCHS,
        SMOKE_STEPS,
        SMOKE_CAP,
        Dialect::SqliteMemory,
    );
    let report = chaos_soak::run(config).await;
    assert_green(&report);
}

/// The short profile on PostgreSQL, with lock-timeout storms and database
/// restarts applied.
#[tokio::test]
#[ignore = "requires PostgreSQL; select inside a with-service.sh pg gate"]
async fn chaos_soak_on_postgres() {
    let dialect = Dialect::postgres_from_env().expect("LASH_POSTGRES_DATABASE_URL names a server");
    let config = SoakConfig::from_env(SMOKE_SEED, SMOKE_EPOCHS, SMOKE_STEPS, SMOKE_CAP, dialect);
    let report = chaos_soak::run(config).await;
    assert_green(&report);
}

/// The long profile on SQLite memory.
#[tokio::test]
#[ignore = "the long profile: an hour of wall time; release certification selects it"]
async fn chaos_soak_long() {
    let config = SoakConfig::from_env(
        LONG_SEED,
        LONG_EPOCHS,
        LONG_STEPS,
        LONG_CAP,
        Dialect::SqliteMemory,
    );
    let report = chaos_soak::run(config).await;
    assert_green(&report);
}

/// The long profile on PostgreSQL, with lock-timeout storms and database
/// restarts applied.
#[tokio::test]
#[ignore = "the long profile on PostgreSQL: an hour of wall time; select inside a with-service.sh pg gate"]
async fn chaos_soak_long_on_postgres() {
    let dialect = Dialect::postgres_from_env().expect("LASH_POSTGRES_DATABASE_URL names a server");
    let config = SoakConfig::from_env(LONG_SEED, LONG_EPOCHS, LONG_STEPS, LONG_CAP, dialect);
    let report = chaos_soak::run(config).await;
    assert_green(&report);
}
