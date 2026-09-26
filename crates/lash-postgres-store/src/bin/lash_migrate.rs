//! `lash-migrate` — provisions and advances the PostgreSQL component schema
//! (FIG-3816).
//!
//! The operational step that must run before workers of a new build open the
//! database: worker startup verifies the schema and never runs DDL, so this
//! binary is the one that creates it on a fresh deployment and applies pending
//! expand-phase migrations on an existing one.
//!
//! ```text
//! lash-migrate [--phase expand|backfill|contract] [--dry-run]
//! ```
//!
//! `LASH_POSTGRES_DATABASE_URL` names the database (its `search_path` chooses
//! the schema the same way a worker connection's does). `--dry-run` prints the
//! plan and changes nothing. `backfill` and `contract` are refused until the
//! operations arc lands (FIG-3817).

// This binary *is* the host: reading args and the environment is its job, so
// the workspace ambient-access ban does not apply to it (FIG-2971).
#![allow(clippy::disallowed_methods)]

use anyhow::{Context, bail};
use lash_postgres_store::{MigrationPhase, MigrationReport, PostgresStorage};

fn usage() -> anyhow::Result<()> {
    bail!(
        "usage: lash-migrate [--phase expand|backfill|contract] [--dry-run] \
         — provisions or advances the PostgreSQL schema for the database named \
         by LASH_POSTGRES_DATABASE_URL"
    )
}

/// The steps a report lists, rendered one per line so the operator log reads
/// as a ledger itself.
fn print_report(report: &MigrationReport) {
    match &report.namespace {
        Some(namespace) => println!("installation: {namespace}"),
        None => println!("installation: none provisioned"),
    }
    match report.found_version {
        Some(version) => println!("found component version: {version}"),
        None => println!("found component version: none"),
    }
    for step in &report.applied {
        println!(
            "applied (earlier run): {} {} ({} -> {}) [{}, {}]",
            step.phase,
            step.migration,
            step.from_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            step.to_version,
            step.release,
            step.state,
        );
    }
    for step in &report.executed {
        println!(
            "applied: {} {} ({} -> {}) [{}, {}]",
            step.phase,
            step.migration,
            step.from_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            step.to_version,
            step.release,
            step.state,
        );
    }
    for step in &report.planned {
        println!(
            "pending: {} {} ({} -> {})",
            step.phase,
            step.migration,
            step.from_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            step.to_version
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut phase = MigrationPhase::Expand;
    let mut dry_run = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--phase" => {
                let name = args.next().context("--phase needs a value")?;
                phase = MigrationPhase::parse(&name)
                    .with_context(|| format!("unknown migration phase `{name}`"))?;
            }
            "--dry-run" => dry_run = true,
            _ => return usage(),
        }
    }
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .context("LASH_POSTGRES_DATABASE_URL must name the database to migrate")?;

    let report = if dry_run {
        PostgresStorage::plan_migrations(&database_url, phase).await
    } else {
        PostgresStorage::migrate(&database_url, phase).await
    }
    .with_context(|| format!("lash-migrate --phase {} failed", phase.name()))?;
    print_report(&report);
    if dry_run {
        println!("dry run: no changes made");
    } else if report.executed.is_empty() {
        println!("nothing to apply: schema is current");
    }
    Ok(())
}
