//! Operator commands for migrations, generation drains and compatibility checks.

// This binary reads argv and the environment on behalf of the operator.
#![allow(clippy::disallowed_methods)]

use lash_core_execution::ClockWallTime;
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::generation_drain::GenerationDrainStatus;
use lash_core_store::compat::{CompatRefusal, DESCRIPTORS};
use lash_core_store::store::{
    FLEET_WRITABLE_RANGE, FleetFormatState, StorePreflight, StoreReleaseState, StoreSchemaOutcome,
    StoreSchemaVerdict,
};
use lash_core_store::store::{ObligationKey, ObligationKind, StalledObligation, StoreError};
use lash_postgres_store::{
    MigrationPhase, MigrationReport, MigrationStep, PostgresStorage, PostgresStorePreflight,
};
use serde::Serialize;
use serde_json::{Value, json};

const LASHCTL_JSON_SCHEMA_VERSION: u32 = 1;
/// The most stalled obligations `drain-status` lists per kind, first by id;
/// `stalled_obligations` still counts every one.
const STALLED_LISTED_PER_KIND: std::num::NonZeroUsize = std::num::NonZeroUsize::new(100).unwrap();
const USAGE: &str = "usage: lashctl [--json] <migrate [--phase expand|backfill|contract] [--dry-run] | drain <generation> | drain-status <generation> | end-drain <generation> | preflight | version>";

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
    refusal: Option<CompatRefusal>,
}

impl CliError {
    fn new(exit: Exit, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: message.into(),
            refusal: None,
        }
    }

    fn store(error: StoreError) -> Self {
        let exit = match &error {
            StoreError::Incompatible { .. } | StoreError::WriterFenced { .. } => Exit::Incompatible,
            _ => Exit::Unexpected,
        };
        let refusal = match &error {
            StoreError::Incompatible { refusal } => Some(refusal.clone()),
            _ => None,
        };
        Self {
            exit,
            message: error.to_string(),
            refusal,
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
    Preflight,
    Version,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Migrate { .. } => "migrate",
            Self::Drain { .. } => "drain",
            Self::DrainStatus { .. } => "drain-status",
            Self::EndDrain { .. } => "end-drain",
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
        }
    }
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
            if *phase != MigrationPhase::Expand {
                return Err(CliError::new(
                    Exit::Refused,
                    format!(
                        "{} migrations are not supported by this build",
                        phase.name()
                    ),
                ));
            }
            let report = if *dry_run {
                PostgresStorage::plan_migrations(&url, *phase).await
            } else {
                PostgresStorage::migrate(&url, *phase).await
            }
            .map_err(CliError::store)?;
            (migration_result(&report, *dry_run), Exit::Done)
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

    #[test]
    fn incompatible_store_error_keeps_the_typed_refusal() {
        let error = CliError::store(StoreError::Incompatible {
            refusal: CompatRefusal::Unstamped {
                component: "postgres".to_string(),
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
