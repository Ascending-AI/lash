//! Operator commands for migrations, generation drains, finalize and
//! compatibility checks.

// This binary reads argv and the environment on behalf of the operator.
#![allow(clippy::disallowed_methods)]

use lash_core_execution::ClockWallTime;
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::generation_drain::GenerationDrainStatus;
use lash_core_store::compat::DESCRIPTORS;
use lash_core_store::store::fleet_finalize::{
    FinalizeError, FinalizeHold, FinalizeMode, FinalizeRefusal,
};
use lash_core_store::store::{
    FLEET_WRITABLE_RANGE, FleetFormatState, StorePreflight, StoreReleaseState, StoreSchemaOutcome,
    StoreSchemaVerdict,
};
use lash_core_store::store::{ObligationKey, ObligationKind, StalledObligation, StoreError};
use lash_postgres_store::{
    FinalizeReport, MigrateError, MigrationPhase, MigrationReport, MigrationStep, PostgresStorage,
    PostgresStorePreflight,
};
use serde::Serialize;
use serde_json::{Value, json};

const LASHCTL_JSON_SCHEMA_VERSION: u32 = 1;
/// The most stalled obligations `drain-status` lists per kind, first by id;
/// `stalled_obligations` still counts every one.
const STALLED_LISTED_PER_KIND: std::num::NonZeroUsize = std::num::NonZeroUsize::new(100).unwrap();
const USAGE: &str = "usage: lashctl [--json] <migrate [--phase expand|backfill|contract] [--dry-run] | drain <generation> | drain-status <generation> | end-drain <generation> | finalize <retired-generation> --restate-admin-url <url> [--override-hold] | finalize-hold show | finalize-hold set --reason <text> | finalize-hold clear | preflight | version>";

#[derive(Clone, Copy)]
enum Exit {
    Done = 0,
    Unexpected = 1,
    Usage = 2,
    Refused = 3,
    Incompatible = 4,
    NotYet = 5,
}

impl Exit {
    fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Unexpected => "unexpected_failure",
            Self::Usage => "usage",
            Self::Refused => "refused_precondition",
            Self::Incompatible => "incompatible_store",
            Self::NotYet => "not_yet",
        }
    }
}

struct CliError {
    exit: Exit,
    message: String,
    /// The typed refusal, as its tagged JSON: a store's `CompatRefusal`, or
    /// a finalize or migration precondition.
    refusal: Option<Value>,
}

impl CliError {
    fn new(exit: Exit, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: message.into(),
            refusal: None,
        }
    }

    fn refused(exit: Exit, message: String, refusal: &impl Serialize) -> Self {
        Self {
            exit,
            message,
            refusal: serde_json::to_value(refusal).ok(),
        }
    }

    fn store(error: StoreError) -> Self {
        let exit = match &error {
            StoreError::Incompatible { .. } | StoreError::WriterFenced { .. } => Exit::Incompatible,
            _ => Exit::Unexpected,
        };
        let refusal = match &error {
            StoreError::Incompatible { refusal } => serde_json::to_value(refusal).ok(),
            _ => None,
        };
        Self {
            exit,
            message: error.to_string(),
            refusal,
        }
    }

    /// A refused migration step is a refused precondition, exit 3.
    fn migrate(error: MigrateError) -> Self {
        match error {
            MigrateError::Refused(refusal) => {
                Self::refused(Exit::Refused, refusal.to_string(), &refusal)
            }
            MigrateError::Store(error) => Self::store(error),
        }
    }

    /// An undrained generation is a drain still pending, exit 5; a retained
    /// deployment or an operator hold is a refused precondition, exit 3. A
    /// deployment registry that cannot be read fails closed.
    fn finalize(error: FinalizeError) -> Self {
        match error {
            FinalizeError::Refused(refusal) => {
                let exit = match &refusal {
                    FinalizeRefusal::GenerationNotDrained { .. } => Exit::NotYet,
                    _ => Exit::Refused,
                };
                Self::refused(exit, refusal.to_string(), &refusal)
            }
            FinalizeError::Registry(error) => Self::new(Exit::Unexpected, error.to_string()),
            FinalizeError::Store(error) => Self::store(error),
        }
    }
}

enum Command {
    Migrate {
        phase: MigrationPhase,
        dry_run: bool,
    },
    Drain {
        generation: BuildGeneration,
    },
    DrainStatus {
        generation: BuildGeneration,
    },
    EndDrain {
        generation: BuildGeneration,
    },
    Finalize {
        retired: BuildGeneration,
        restate_admin_url: String,
        mode: FinalizeMode,
    },
    FinalizeHold(HoldAction),
    Preflight,
    Version,
}

enum HoldAction {
    Show,
    Set { reason: String },
    Clear,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Migrate { .. } => "migrate",
            Self::Drain { .. } => "drain",
            Self::DrainStatus { .. } => "drain-status",
            Self::EndDrain { .. } => "end-drain",
            Self::Finalize { .. } => "finalize",
            Self::FinalizeHold(_) => "finalize-hold",
            Self::Preflight => "preflight",
            Self::Version => "version",
        }
    }
}

struct Invocation {
    command: Command,
    json: bool,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Invocation, CliError> {
    let mut args = args.into_iter().peekable();
    let mut json = false;
    let mut words = Vec::new();
    for arg in &mut args {
        if arg == "--json" {
            if json {
                return Err(CliError::new(Exit::Usage, USAGE));
            }
            json = true;
        } else {
            words.push(arg);
        }
    }
    let Some(verb) = words.first().map(String::as_str) else {
        return Err(CliError::new(Exit::Usage, USAGE));
    };
    let rest = &words[1..];
    let command = match verb {
        "migrate" => {
            let mut phase = MigrationPhase::Expand;
            let mut dry_run = false;
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--phase" if index + 1 < rest.len() => {
                        phase = MigrationPhase::parse(&rest[index + 1])
                            .ok_or_else(|| CliError::new(Exit::Usage, USAGE))?;
                        index += 2;
                    }
                    "--dry-run" if !dry_run => {
                        dry_run = true;
                        index += 1;
                    }
                    _ => return Err(CliError::new(Exit::Usage, USAGE)),
                }
            }
            Command::Migrate { phase, dry_run }
        }
        "drain" | "drain-status" | "end-drain" if rest.len() == 1 => {
            let generation = BuildGeneration::parse(&rest[0])
                .map_err(|_| CliError::new(Exit::Usage, "invalid build generation"))?;
            match verb {
                "drain" => Command::Drain { generation },
                "drain-status" => Command::DrainStatus { generation },
                _ => Command::EndDrain { generation },
            }
        }
        "finalize" if !rest.is_empty() => {
            let retired = BuildGeneration::parse(&rest[0])
                .map_err(|_| CliError::new(Exit::Usage, "invalid build generation"))?;
            let mut restate_admin_url = None;
            let mut mode = FinalizeMode::Automatic;
            let mut index = 1;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--restate-admin-url"
                        if index + 1 < rest.len() && restate_admin_url.is_none() =>
                    {
                        restate_admin_url = Some(rest[index + 1].clone());
                        index += 2;
                    }
                    "--override-hold" if mode == FinalizeMode::Automatic => {
                        mode = FinalizeMode::OverrideHold;
                        index += 1;
                    }
                    _ => return Err(CliError::new(Exit::Usage, USAGE)),
                }
            }
            let restate_admin_url = restate_admin_url.ok_or_else(|| {
                CliError::new(
                    Exit::Usage,
                    "finalize needs --restate-admin-url: retirement is read from the engine's deployments",
                )
            })?;
            Command::Finalize {
                retired,
                restate_admin_url,
                mode,
            }
        }
        "finalize-hold" => match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["show"] => Command::FinalizeHold(HoldAction::Show),
            ["clear"] => Command::FinalizeHold(HoldAction::Clear),
            ["set", "--reason", reason] if !reason.trim().is_empty() => {
                Command::FinalizeHold(HoldAction::Set {
                    reason: reason.to_owned(),
                })
            }
            _ => return Err(CliError::new(Exit::Usage, USAGE)),
        },
        "preflight" if rest.is_empty() => Command::Preflight,
        "version" if rest.is_empty() => Command::Version,
        _ => return Err(CliError::new(Exit::Usage, USAGE)),
    };
    Ok(Invocation { command, json })
}

#[derive(Serialize)]
struct StepDto<'a> {
    phase: &'a str,
    migration: &'a str,
    release: &'a str,
    state: &'a str,
    from_version: Option<i32>,
    to_version: i32,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    backfill_cursor: Option<&'a str>,
    backfill_rows: Option<i64>,
}

impl<'a> From<&'a MigrationStep> for StepDto<'a> {
    fn from(step: &'a MigrationStep) -> Self {
        Self {
            phase: &step.phase,
            migration: &step.migration,
            release: &step.release,
            state: &step.state,
            from_version: step.from_version,
            to_version: step.to_version,
            started_at_ms: step.started_at_ms,
            finished_at_ms: step.finished_at_ms,
            backfill_cursor: step.backfill_cursor.as_deref(),
            backfill_rows: step.backfill_rows,
        }
    }
}

fn finalize_result(report: &FinalizeReport) -> Value {
    json!({
        "retired_generation": report.drain.generation.as_str(),
        "flip": report.flip,
        "fleet_format": report.flip.fleet(),
        "backfills": report.backfills.iter().map(StepDto::from).collect::<Vec<_>>(),
    })
}

fn hold_result(hold: Option<&FinalizeHold>) -> Value {
    json!({
        "held": hold.is_some(),
        "reason": hold.map(|hold| hold.reason.as_str()),
        "held_at_ms": hold.map(|hold| hold.held_at_ms),
    })
}

fn migration_result(report: &MigrationReport, dry_run: bool) -> Value {
    json!({
        "namespace": report.namespace,
        "found_version": report.found_version,
        "dry_run": dry_run,
        "applied": report.applied.iter().map(StepDto::from).collect::<Vec<_>>(),
        "executed": report.executed.iter().map(StepDto::from).collect::<Vec<_>>(),
        "planned": report.planned.iter().map(StepDto::from).collect::<Vec<_>>(),
    })
}

/// The row a stalled obligation lives on, by its key's column names, or
/// `None` when this build cannot name it.
fn stalled_row(key: &ObligationKey) -> Value {
    match key {
        ObligationKey::Ingress {
            session_id,
            item_id,
        } => json!({"session_id":session_id.as_str(),"item_id":item_id}),
        ObligationKey::ControlIntent { intent_id } => json!({"intent_id":intent_id.sequence()}),
        ObligationKey::ScopeClose { session_id, root } => {
            json!({"session_id":session_id.as_str(),"root":root.as_str()})
        }
        ObligationKey::ParentEnd {
            parent_kind,
            parent_id,
        } => json!({"parent_kind":parent_kind,"parent_id":parent_id}),
        ObligationKey::SessionDelete { session_id } => json!({"session_id":session_id.as_str()}),
        ObligationKey::TriggerDelivery {
            occurrence_id,
            subscription_id,
        } => json!({"occurrence_id":occurrence_id,"subscription_id":subscription_id}),
        ObligationKey::ProcessStart { process_id }
        | ObligationKey::ProcessTerminal { process_id } => {
            json!({"process_id":process_id.as_str()})
        }
        ObligationKey::ArtifactCleanup { referrer } => {
            json!({"referrer_kind":referrer.kind().as_str(),"referrer_id":referrer.canonical_id()})
        }
    }
}

fn stalled_result(stalled: &StalledObligation) -> Value {
    let (row, undecodable) = match &stalled.key {
        Ok(key) => (stalled_row(key), None),
        Err(error) => (Value::Null, Some(error.detail.as_str())),
    };
    json!({
        "kind": stalled.kind.label(),
        "obligation_id": stalled.id.as_str(),
        "reason": stalled.reason.as_str(),
        "row": row,
        "undecodable": undecodable,
        "attempts": stalled.attempts,
        "last_error": stalled.last_error,
        "stalled_at_ms": stalled.stalled_at_ms,
    })
}

fn drain_status_result(status: &GenerationDrainStatus, stalled: &[StalledObligation]) -> Value {
    json!({
        "generation": status.generation.as_str(),
        "draining_since_ms": status.draining_since_ms,
        "live_processes": status.live_processes,
        "parked_processes": status.parked_processes,
        "parked_turns": status.parked_turns,
        "in_flight_turns": status.in_flight_turns,
        "closing_sessions": status.closing_sessions,
        "stalled_obligations": status.stalled_obligations.iter().map(|(kind, count)| (kind.label(), *count)).collect::<std::collections::BTreeMap<_, _>>(),
        "stalled": stalled.iter().map(stalled_result).collect::<Vec<_>>(),
        "drained": status.drained(),
        "checked_at_ms": status.checked_at,
    })
}

fn version_result(fleet_generations: &[(BuildGeneration, bool)]) -> Value {
    json!({
        "release": env!("CARGO_PKG_VERSION"),
        "cli_build_generation": lash::formats::build_generation().as_str(),
        "fleet_generations": fleet_generations.iter().map(|(generation, draining)| json!({
            "generation": generation.as_str(),
            "draining": draining,
            "source": "postgres",
        })).collect::<Vec<_>>(),
        "fleet_writable": FLEET_WRITABLE_RANGE,
        "components": DESCRIPTORS.iter().map(|descriptor| json!({
            "component": descriptor.component.as_str(),
            "reads": descriptor.reads,
            "writes": descriptor.writes,
        })).collect::<Vec<_>>(),
        "wires": {
            "remote_protocol": lash_remote_protocol::REMOTE_PROTOCOL,
            "restate": lash_restate::RESTATE_WIRE,
        },
    })
}

fn database_url() -> Result<String, CliError> {
    std::env::var("LASH_POSTGRES_DATABASE_URL").map_err(|_| {
        CliError::new(
            Exit::Refused,
            "LASH_POSTGRES_DATABASE_URL must name the PostgreSQL database",
        )
    })
}

async fn run(command: &Command) -> Result<(Value, Exit), CliError> {
    let url = database_url()?;
    let outcome = match command {
        Command::Version => {
            let storage = PostgresStorage::connect(&url)
                .await
                .map_err(CliError::store)?;
            let generations = storage.fleet_generations().await.map_err(CliError::store)?;
            (version_result(&generations), Exit::Done)
        }
        Command::Migrate { phase, dry_run } => {
            let report = if *dry_run {
                PostgresStorage::plan_migrations(&url, *phase).await
            } else {
                PostgresStorage::migrate(&url, *phase).await
            }
            .map_err(CliError::migrate)?;
            (migration_result(&report, *dry_run), Exit::Done)
        }
        Command::Finalize {
            retired,
            restate_admin_url,
            mode,
        } => {
            let storage = PostgresStorage::connect(&url)
                .await
                .map_err(CliError::store)?;
            let registry = lash_restate::RestateDeploymentRegistry::new(
                lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(
                    restate_admin_url.clone(),
                )),
            );
            let report = storage
                .finalize(
                    retired,
                    &registry,
                    *mode,
                    lash_core_execution::facade_support::SystemClock.timestamp_ms(),
                )
                .await
                .map_err(CliError::finalize)?;
            (finalize_result(&report), Exit::Done)
        }
        Command::FinalizeHold(action) => {
            let storage = PostgresStorage::connect(&url)
                .await
                .map_err(CliError::store)?;
            let result = match action {
                HoldAction::Show => hold_result(
                    storage
                        .finalize_hold()
                        .await
                        .map_err(CliError::store)?
                        .as_ref(),
                ),
                HoldAction::Set { reason } => hold_result(Some(
                    &storage
                        .set_finalize_hold(reason)
                        .await
                        .map_err(CliError::store)?,
                )),
                HoldAction::Clear => {
                    let cleared = storage
                        .clear_finalize_hold()
                        .await
                        .map_err(CliError::store)?;
                    let mut result = hold_result(None);
                    result["cleared"] = hold_result(cleared.as_ref());
                    result
                }
            };
            (result, Exit::Done)
        }
        Command::Preflight => {
            let probe = PostgresStorePreflight::for_database_url(&url).map_err(CliError::store)?;
            let status = probe.schema_status().await.map_err(CliError::store);
            probe.close().await;
            let status = status?;
            let outcome = status.outcome();
            let databases = status.databases.iter().map(|database| {
                let (verdict, found, refusal, reason) = match &database.verdict {
                    StoreSchemaVerdict::Matches => ("matches", None, None, None),
                    StoreSchemaVerdict::Expanded { found } => ("expanded", Some(*found), None, None),
                    StoreSchemaVerdict::Refused { refusal } => ("refused", None, Some(refusal), None),
                    StoreSchemaVerdict::Migratable { found } => ("migratable", Some(*found), None, None),
                    StoreSchemaVerdict::Mismatch { found } => ("mismatch", Some(*found), None, None),
                    StoreSchemaVerdict::Absent => ("absent", None, None, None),
                    StoreSchemaVerdict::Unreadable { reason } => ("unreadable", None, None, Some(reason.as_str())),
                    _ => ("unknown", None, None, None),
                };
                json!({"name":database.name,"location":database.location,"expected":database.expected,"min_reader":database.min_reader,"verdict":verdict,"found":found,"refusal":refusal,"reason":reason})
            }).collect::<Vec<_>>();
            let release = match &status.release {
                StoreReleaseState::Stamped(stamp) => {
                    json!({"state":"stamped","release":stamp.release,"written_at_ms":stamp.written_at_epoch_ms})
                }
                StoreReleaseState::Unstamped => json!({"state":"unstamped"}),
                StoreReleaseState::Unreadable { reason } => {
                    json!({"state":"unreadable","reason":reason})
                }
                _ => json!({"state":"unknown"}),
            };
            let fleet = match &status.fleet_format {
                FleetFormatState::Recorded(value) => {
                    json!({"state":"recorded","version":value.version()})
                }
                FleetFormatState::Unrecorded => json!({"state":"unrecorded"}),
                FleetFormatState::Unreadable { reason } => {
                    json!({"state":"unreadable","reason":reason})
                }
                _ => json!({"state":"unknown"}),
            };
            let exit = match outcome {
                StoreSchemaOutcome::Ready => Exit::Done,
                StoreSchemaOutcome::Refused => Exit::Incompatible,
                StoreSchemaOutcome::Undecided => Exit::Refused,
                _ => Exit::Refused,
            };
            (
                json!({"outcome":exit.name(),"databases":databases,"release":release,"fleet_format":fleet}),
                exit,
            )
        }
        Command::Drain { generation }
        | Command::EndDrain { generation }
        | Command::DrainStatus { generation } => {
            let storage = PostgresStorage::connect(&url)
                .await
                .map_err(CliError::store)?;
            let drain = storage.generation_drain();
            match command {
                Command::Drain { .. } => {
                    let changed = drain
                        .mark_draining(
                            generation,
                            lash_core_execution::facade_support::SystemClock.timestamp_ms(),
                        )
                        .await
                        .map_err(CliError::store)?;
                    (
                        json!({"generation":generation.as_str(),"marked":changed}),
                        Exit::Done,
                    )
                }
                Command::EndDrain { .. } => {
                    let changed = drain
                        .clear_draining(generation)
                        .await
                        .map_err(CliError::store)?;
                    (
                        json!({"generation":generation.as_str(),"cleared":changed}),
                        Exit::Done,
                    )
                }
                Command::DrainStatus { .. } => {
                    let status = GenerationDrainStatus::collect(
                        drain.as_ref(),
                        storage.session_delete_ledger().as_ref(),
                        |kind| storage.obligation_ledger(kind),
                        generation,
                        lash_core_execution::facade_support::SystemClock.timestamp_ms(),
                    )
                    .await
                    .map_err(CliError::store)?;
                    // Stalled obligations never hold the drain, so each is
                    // listed for the operator to settle before retirement.
                    let mut stalled = Vec::new();
                    for kind in ObligationKind::ALL {
                        if status.stalled_obligations.get(&kind).copied().unwrap_or(0) > 0 {
                            stalled.extend(
                                storage
                                    .obligation_ledger(kind)
                                    .list_stalled(None, STALLED_LISTED_PER_KIND)
                                    .await
                                    .map_err(CliError::store)?,
                            );
                        }
                    }
                    let exit = if status.drained() {
                        Exit::Done
                    } else {
                        Exit::NotYet
                    };
                    (drain_status_result(&status, &stalled), exit)
                }
                _ => unreachable!("matched a drain command"),
            }
        }
    };
    Ok(outcome)
}

fn output(command: &str, result: Option<Value>, error: Option<&CliError>, json_mode: bool) -> Exit {
    let exit = error.map_or(Exit::Done, |error| error.exit);
    if json_mode {
        let error = error.map(error_json);
        println!(
            "{}",
            json!({"schema_version":LASHCTL_JSON_SCHEMA_VERSION,"command":command,"result":result,"error":error})
        );
    } else if let Some(error) = error {
        eprintln!("lashctl: {}", error.message);
    } else if let Some(result) = result {
        if command == "version" {
            println!(
                "release: {}",
                result["release"].as_str().unwrap_or("unknown")
            );
            println!(
                "CLI build generation: {}",
                result["cli_build_generation"].as_str().unwrap_or("unknown")
            );
            println!("fleet generations:");
            if let Some(generations) = result["fleet_generations"].as_array() {
                for generation in generations {
                    println!(
                        "  {} (draining: {}, source: {})",
                        generation["generation"].as_str().unwrap_or("unknown"),
                        generation["draining"].as_bool().unwrap_or(false),
                        generation["source"].as_str().unwrap_or("unknown"),
                    );
                }
            }
            println!(
                "compatibility: {}",
                json!({"fleet_writable":result["fleet_writable"],"components":result["components"],"wires":result["wires"]})
            );
        } else {
            println!("{result:#}");
        }
    }
    exit
}

fn error_json(error: &CliError) -> Value {
    json!({"code":error.exit.name(),"message":error.message,"refusal":error.refusal})
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let json_mode = args.iter().any(|arg| arg == "--json");
    let parsed = parse(args);
    let exit = match parsed {
        Ok(invocation) => {
            let command = invocation.command.name();
            match run(&invocation.command).await {
                Ok((result, status)) => {
                    let status_error = match status {
                        Exit::Done => None,
                        Exit::NotYet => {
                            Some(CliError::new(status, "the generation is not yet drained"))
                        }
                        Exit::Incompatible => Some(CliError::new(
                            status,
                            "the store schema is incompatible with this build",
                        )),
                        _ => Some(CliError::new(status, "the operation did not complete")),
                    };
                    output(
                        command,
                        Some(result),
                        status_error.as_ref(),
                        invocation.json,
                    );
                    status
                }
                Err(error) => output(command, None, Some(&error), invocation.json),
            }
        }
        Err(error) => output("invalid", None, Some(&error), json_mode),
    };
    std::process::ExitCode::from(exit as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::compat::CompatRefusal;

    #[test]
    fn incompatible_store_error_keeps_the_typed_refusal() {
        let error = CliError::store(StoreError::Incompatible {
            refusal: CompatRefusal::Unstamped {
                component: "postgres".to_string(),
                writing_release: None,
            },
        });
        assert_eq!(error.exit as u8, 4);
        assert_eq!(
            error_json(&error)["refusal"],
            json!({"refusal":"unstamped","component":"postgres"})
        );
    }

    /// A store `lashctl migrate` never seeded refuses every opening command
    /// as incompatible, and the refusal names the remedy (FIG-4075).
    #[test]
    fn a_store_without_a_fleet_epoch_is_refused_toward_migrate() {
        let error = CliError::store(StoreError::Incompatible {
            refusal: CompatRefusal::FleetUnrecorded {
                component: "postgres".to_string(),
                writing_release: None,
            },
        });
        assert_eq!(error.exit as u8, 4);
        assert_eq!(
            error_json(&error)["refusal"],
            json!({"refusal":"fleet_unrecorded","component":"postgres"})
        );
        assert!(
            error.message.contains("run `lashctl migrate`"),
            "{}",
            error.message
        );
    }

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn finalize_names_its_retired_generation_the_engine_and_the_hold_override() {
        let parsed = parse(words(&[
            "finalize",
            "0123456789ab",
            "--restate-admin-url",
            "http://admin",
            "--override-hold",
        ]))
        .unwrap_or_else(|error| panic!("{}", error.message));
        match parsed.command {
            Command::Finalize {
                retired,
                restate_admin_url,
                mode,
            } => {
                assert_eq!(retired.as_str(), "0123456789ab");
                assert_eq!(restate_admin_url, "http://admin");
                assert_eq!(mode, FinalizeMode::OverrideHold);
            }
            _ => panic!("finalize parses to Finalize"),
        }
        let automatic = parse(words(&[
            "finalize",
            "0123456789ab",
            "--restate-admin-url",
            "http://admin",
        ]))
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(matches!(
            automatic.command,
            Command::Finalize {
                mode: FinalizeMode::Automatic,
                ..
            }
        ));
        for refused in [
            words(&["finalize"]),
            words(&["finalize", "0123456789ab"]),
            words(&["finalize", "0123456789ab", "--restate-admin-url"]),
            words(&[
                "finalize",
                "0123456789ab",
                "--restate-admin-url",
                "a",
                "--restate-admin-url",
                "b",
            ]),
            words(&["finalize-hold"]),
            words(&["finalize-hold", "set", "--reason"]),
            words(&["finalize-hold", "clear", "now"]),
        ] {
            let Err(error) = parse(refused.clone()) else {
                panic!("{refused:?} must be a usage error");
            };
            assert_eq!(error.exit as u8, 2, "{refused:?}");
        }
        assert!(matches!(
            parse(words(&["finalize-hold", "set", "--reason", "watch"]))
                .unwrap_or_else(|error| panic!("{}", error.message))
                .command,
            Command::FinalizeHold(HoldAction::Set { reason }) if reason == "watch"
        ));
    }

    /// An undrained generation is a drain still pending (exit 5); a retained
    /// deployment and a hold are refused preconditions (exit 3); an engine
    /// that cannot be read fails closed (exit 1). Each keeps its typed
    /// refusal.
    #[test]
    fn finalize_refusals_keep_their_exit_codes_and_types() {
        use lash_core_store::store::fleet_finalize::DeploymentRegistryError;
        let held = CliError::finalize(FinalizeError::Refused(FinalizeRefusal::Held {
            hold: FinalizeHold {
                reason: "watch".to_owned(),
                held_at_ms: 3,
            },
        }));
        assert_eq!(held.exit as u8, 3);
        assert_eq!(
            error_json(&held)["refusal"],
            json!({"refusal":"held","hold":{"reason":"watch","held_at_ms":3}})
        );
        let retained = CliError::finalize(FinalizeError::Refused(
            FinalizeRefusal::DeploymentsRetained {
                generation: BuildGeneration::for_test("lashctl-retained"),
                deployments: Vec::new(),
            },
        ));
        assert_eq!(retained.exit as u8, 3);
        let registry = CliError::finalize(FinalizeError::Registry(DeploymentRegistryError {
            detail: "connection refused".to_owned(),
        }));
        assert_eq!(registry.exit as u8, 1);
        assert_eq!(error_json(&registry)["refusal"], Value::Null);
    }

    #[test]
    fn a_stalled_obligation_lists_its_identity_and_typed_reason() {
        use lash_core_store::store::{KeyColumn, ObligationId, StallReason, UndecodableObligation};

        let decoded = StalledObligation {
            kind: ObligationKind::ControlIntent,
            id: ObligationId::new("obligation-decoded"),
            key: ObligationKey::decode(ObligationKind::ControlIntent, vec![KeyColumn::Integer(7)]),
            reason: StallReason::AttemptsExhausted,
            attempts: 3,
            last_error: Some("the engine was unavailable".to_owned()),
            stalled_at_ms: 11,
        };
        assert_eq!(
            stalled_result(&decoded),
            json!({
                "kind": "control_intent",
                "obligation_id": "obligation-decoded",
                "reason": "attempts_exhausted",
                "row": {"intent_id": 7},
                "undecodable": null,
                "attempts": 3,
                "last_error": "the engine was unavailable",
                "stalled_at_ms": 11,
            })
        );

        let foreign = StalledObligation {
            kind: ObligationKind::ArtifactCleanup,
            id: ObligationId::new("obligation-foreign"),
            key: Err(UndecodableObligation {
                detail: "unknown artifact referrer kind `synthetic_next`".to_owned(),
            }),
            reason: StallReason::Undecodable,
            attempts: 1,
            last_error: None,
            stalled_at_ms: 12,
        };
        let listed = stalled_result(&foreign);
        assert_eq!(listed["reason"], "undecodable");
        assert_eq!(listed["row"], Value::Null);
        assert_eq!(
            listed["undecodable"],
            "unknown artifact referrer kind `synthetic_next`"
        );
    }
}
