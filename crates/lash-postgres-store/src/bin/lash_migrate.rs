//! `lash-migrate` — provisions and advances the PostgreSQL component schema
//! (FIG-3816), and operates a build generation's drain (FIG-3884).
//!
//! The operational step that must run before workers of a new build open the
//! database: worker startup verifies the schema and never runs DDL, so this
//! binary is the one that creates it on a fresh deployment and applies pending
//! expand-phase migrations on an existing one.
//!
//! ```text
//! lash-migrate [--phase expand|backfill|contract] [--dry-run]
//! lash-migrate drain <generation> [--json]
//! lash-migrate drain-status <generation> [--json]
//! lash-migrate end-drain <generation> [--json]
//! ```
//!
//! `LASH_POSTGRES_DATABASE_URL` names the database (its `search_path` chooses
//! the schema the same way a worker connection's does). `--dry-run` prints the
//! plan and changes nothing. `backfill` and `contract` are refused until the
//! operations arc lands (FIG-3817).
//!
//! The drain verbs write and read the same store ports the facade's
//! `LashCore::{drain_generation, generation_drain_status, end_generation_drain}`
//! compose: `drain` marks a generation draining, `end-drain` clears the mark,
//! and `drain-status` reports the generation's live processes, parked work,
//! unsettled admitted turns and stalled obligations. The facade's
//! `DrainOwnGeneration` guard names the calling deployment's build; this
//! binary is not a deployment, so the guard does not apply — an operator may
//! drain any generation the store set knows.

// This binary *is* the host: reading args and the environment is its job, so
// the workspace ambient-access ban does not apply to it (FIG-2971).
#![allow(clippy::disallowed_methods)]

use anyhow::{Context, bail};
use lash_core_execution::ClockWallTime;
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::generation_drain::GenerationDrainStatus;
use lash_postgres_store::{MigrationPhase, MigrationReport, PostgresStorage};

fn usage() -> anyhow::Result<Command> {
    bail!(
        "usage: lash-migrate [--phase expand|backfill|contract] [--dry-run] \
         | lash-migrate drain <generation> [--json] \
         | lash-migrate drain-status <generation> [--json] \
         | lash-migrate end-drain <generation> [--json] \
         — provisions or advances the PostgreSQL schema for the database named \
         by LASH_POSTGRES_DATABASE_URL, or operates a build generation's drain \
         against it"
    )
}

/// The mode the argv selects: schema migration, or one of the generation
/// drain verbs (FIG-3884).
enum Command {
    Migrate {
        phase: MigrationPhase,
        dry_run: bool,
    },
    Drain {
        generation: BuildGeneration,
        json: bool,
    },
    DrainStatus {
        generation: BuildGeneration,
        json: bool,
    },
    EndDrain {
        generation: BuildGeneration,
        json: bool,
    },
}

fn parse_generation(text: Option<String>) -> anyhow::Result<BuildGeneration> {
    let text = text.context("the drain commands take a build generation")?;
    BuildGeneration::parse(&text).with_context(|| format!("`{text}` is not a build generation"))
}

fn parse_args(args: impl Iterator<Item = String>) -> anyhow::Result<Command> {
    let mut args = args;
    // A drain verb is the first word; anything else starts the migrate flags.
    let mut first = match args.next() {
        None => {
            return Ok(Command::Migrate {
                phase: MigrationPhase::Expand,
                dry_run: false,
            });
        }
        Some(first) => first,
    };
    if matches!(first.as_str(), "drain" | "drain-status" | "end-drain") {
        let generation = parse_generation(args.next())?;
        let mut json = false;
        for arg in args {
            match arg.as_str() {
                "--json" => json = true,
                _ => return usage(),
            }
        }
        return Ok(match first.as_str() {
            "drain" => Command::Drain { generation, json },
            "drain-status" => Command::DrainStatus { generation, json },
            _ => Command::EndDrain { generation, json },
        });
    }
    let mut phase = MigrationPhase::Expand;
    let mut dry_run = false;
    loop {
        match first.as_str() {
            "--phase" => {
                let name = args.next().context("--phase needs a value")?;
                phase = MigrationPhase::parse(&name)
                    .with_context(|| format!("unknown migration phase `{name}`"))?;
            }
            "--dry-run" => dry_run = true,
            _ => return usage(),
        }
        match args.next() {
            Some(arg) => first = arg,
            None => break,
        }
    }
    Ok(Command::Migrate { phase, dry_run })
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

/// The human-readable form of one generation's drain status: one line per
/// count, in the serialized report's own vocabulary.
fn print_drain_status(status: &GenerationDrainStatus) {
    println!("generation: {}", status.generation);
    match status.draining_since_ms {
        Some(since) => println!("draining since (ms): {since}"),
        None => println!("draining since (ms): not marked"),
    }
    println!("live processes: {}", status.live_processes);
    println!("parked processes: {}", status.parked_processes);
    println!("parked turns: {}", status.parked_turns);
    println!("in-flight turns: {}", status.in_flight_turns);
    let stalled = status
        .stalled_obligations
        .iter()
        .map(|(kind, count)| format!("{}={count}", kind.label()))
        .collect::<Vec<_>>()
        .join(" ");
    println!("stalled obligations: {stalled}");
    println!("drained: {}", status.drained());
    println!("checked at (ms): {}", status.checked_at);
}

/// Open the catalog the drain verbs act on: a worker-style connect that
/// verifies the schema is current, never DDL (FIG-3816).
async fn open(database_url: &str) -> anyhow::Result<PostgresStorage> {
    PostgresStorage::connect(database_url)
        .await
        .context("lash-migrate could not open the database")
}

/// The write half of the drain verbs: `mark` selects the `drain`/`end-drain`
/// side, and `changed` reports whether the mark moved.
async fn drain_change(
    storage: &PostgresStorage,
    generation: &BuildGeneration,
    mark: bool,
    json: bool,
) -> anyhow::Result<()> {
    let drain = storage.generation_drain();
    let changed = if mark {
        drain
            .mark_draining(
                generation,
                lash_core_execution::facade_support::SystemClock.timestamp_ms(),
            )
            .await
    } else {
        drain.clear_draining(generation).await
    }
    .with_context(|| {
        format!(
            "lash-migrate {} {generation} failed",
            if mark { "drain" } else { "end-drain" },
        )
    })?;
    if json {
        let verb = if mark { "marked" } else { "cleared" };
        println!(
            "{}",
            serde_json::json!({ "generation": generation.as_str(), verb: changed })
        );
    } else {
        let line = match (mark, changed) {
            (true, true) => format!("drain marked: {generation}"),
            (true, false) => format!("already draining: {generation}"),
            (false, true) => format!("drain cleared: {generation}"),
            (false, false) => format!("not draining: {generation}"),
        };
        println!("{line}");
    }
    Ok(())
}

async fn run(command: Command, database_url: &str) -> anyhow::Result<()> {
    match command {
        Command::Migrate { phase, dry_run } => {
            let report = if dry_run {
                PostgresStorage::plan_migrations(database_url, phase).await
            } else {
                PostgresStorage::migrate(database_url, phase).await
            }
            .with_context(|| format!("lash-migrate --phase {} failed", phase.name()))?;
            print_report(&report);
            if dry_run {
                println!("dry run: no changes made");
            } else if report.executed.is_empty() {
                println!("nothing to apply: schema is current");
            }
        }
        Command::Drain { generation, json } => {
            let storage = open(database_url).await?;
            drain_change(&storage, &generation, true, json).await?;
        }
        Command::EndDrain { generation, json } => {
            let storage = open(database_url).await?;
            drain_change(&storage, &generation, false, json).await?;
        }
        Command::DrainStatus { generation, json } => {
            let storage = open(database_url).await?;
            let drain = storage.generation_drain();
            let status = GenerationDrainStatus::collect(
                drain.as_ref(),
                |kind| storage.obligation_ledger(kind),
                &generation,
                lash_core_execution::facade_support::SystemClock.timestamp_ms(),
            )
            .await
            .with_context(|| format!("lash-migrate drain-status {generation} failed"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_drain_status(&status);
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = parse_args(std::env::args().skip(1))?;
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .context("LASH_POSTGRES_DATABASE_URL must name the database to migrate")?;
    run(command, &database_url).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> impl Iterator<Item = String> {
        words.iter().map(|word| word.to_string())
    }

    #[test]
    fn bare_invocation_is_an_expand_migration() {
        let command = parse_args(args(&[])).expect("no args parses");
        assert!(matches!(
            command,
            Command::Migrate {
                phase: MigrationPhase::Expand,
                dry_run: false
            }
        ));
    }

    #[test]
    fn migrate_flags_parse() {
        let command =
            parse_args(args(&["--phase", "backfill", "--dry-run"])).expect("migrate flags parse");
        assert!(matches!(
            command,
            Command::Migrate {
                phase: MigrationPhase::Backfill,
                dry_run: true
            }
        ));
    }

    #[test]
    fn drain_verbs_parse_their_generation_and_json_flag() {
        for verb in ["drain", "drain-status", "end-drain"] {
            let command = parse_args(args(&[verb, "0123456789ab", "--json"]))
                .unwrap_or_else(|error| panic!("{verb} parses: {error}"));
            match (verb, &command) {
                ("drain", Command::Drain { generation, json })
                | ("drain-status", Command::DrainStatus { generation, json })
                | ("end-drain", Command::EndDrain { generation, json }) => {
                    assert_eq!(generation.as_str(), "0123456789ab");
                    assert!(*json);
                }
                _ => panic!("{verb} parsed to the wrong command"),
            }
        }
    }

    #[test]
    fn a_drain_verb_without_a_generation_is_usage() {
        assert!(parse_args(args(&["drain"])).is_err());
        assert!(parse_args(args(&["drain-status"])).is_err());
        assert!(parse_args(args(&["end-drain"])).is_err());
    }

    #[test]
    fn a_malformed_generation_is_refused() {
        assert!(parse_args(args(&["drain", "not-a-generation"])).is_err());
    }

    #[test]
    fn unknown_args_are_usage() {
        assert!(parse_args(args(&["frobnicate"])).is_err());
        assert!(parse_args(args(&["drain", "0123456789ab", "--phase"])).is_err());
    }
}
