//! Operator commands for migrations, recovery and compatibility checks.

// This binary reads argv and the environment on behalf of the operator.
#![allow(clippy::disallowed_methods)]

use lash_core_store::compat::DESCRIPTORS;
use lash_core_store::store::{
    FLEET_WRITABLE_RANGE, StorePreflight, StoreSchemaOutcome, StoreSchemaStatus,
};
use lash_core_store::store::{ObligationKey, StalledObligation, StoreError};
use lash_postgres_store::{
    MigrateError, MigrationPhase, MigrationReport, MigrationStep, PostgresConnectionBudget,
    PostgresConnectionBudgetReport, PostgresStorage, PostgresStorePreflight,
};
mod recovery;

use serde::Serialize;
use serde_json::{Value, json};

/// version_guard(
///     shapes(cover(StepDto, PreflightJson)),
///     roots(Exit, CliError),
///     roots(path = "crates/lash-postgres-store/src/postgres/migrate.rs", MigrationRefusal),
///     items(
///         name, from, run, output, error_json, migration_result, stalled_row, stalled_result,
///         version_result, preflight_result,
///     ),
///     shapes(
///         path = "crates/lash-postgres-store/src/connection_budget.rs",
///         cover(PostgresConnectionBudgetReport, PostgresConnectionBudgetRefusal),
///     ),
/// )
/// version_surface = "coexist"
/// format_outside_manifest = "operator CLI wire: gates a --json consumer, not state lash reopens"
const LASHCTL_JSON_SCHEMA_VERSION: u32 = 1;
const OPERATOR_POOL_MAX: u32 = 2;
const USAGE: &str = "usage: lashctl [--json] <migrate [--phase expand|backfill|contract] [--dry-run] | preflight [--processes-per-generation <n> --pool-max <n> --generations <n> --workers <n> --admin-headroom <n>] | park list|events [--after <json>] [--limit <n>] | park redrive|cancel|fork --target <json> --park-id <n> | stalled list <kind> [--after <id>] [--limit <n>] | stalled rearm <kind> <id> | deployment-status --accepting-new-work <bool> (recovery commands accept --sqlite-path <database-file>) | version>";

#[derive(Clone, Copy)]
enum Exit {
    Done = 0,
    Unexpected = 1,
    Usage = 2,
    Refused = 3,
    Incompatible = 4,
}

impl Exit {
    fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Unexpected => "unexpected_failure",
            Self::Usage => "usage",
            Self::Refused => "refused_precondition",
            Self::Incompatible => "incompatible_store",
        }
    }
}

struct CliError {
    exit: Exit,
    message: String,
    /// The typed refusal, as its tagged JSON: a store's `CompatRefusal`, or
    /// a migration precondition.
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
}

enum Command {
    Recovery(recovery::Invocation),
    Migrate {
        phase: MigrationPhase,
        dry_run: bool,
    },
    Preflight {
        budget: Option<PostgresConnectionBudget>,
    },
    Version,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Recovery(invocation) => invocation.command.name(),
            Self::Migrate { .. } => "migrate",
            Self::Preflight { .. } => "preflight",
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
        "park" | "stalled" | "deployment-status" => Command::Recovery(recovery::parse(verb, rest)?),
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
        "preflight" => Command::Preflight {
            budget: parse_connection_budget(rest)?,
        },
        "version" if rest.is_empty() => Command::Version,
        _ => return Err(CliError::new(Exit::Usage, USAGE)),
    };
    Ok(Invocation { command, json })
}

fn parse_connection_budget(rest: &[String]) -> Result<Option<PostgresConnectionBudget>, CliError> {
    if rest.is_empty() {
        return Ok(None);
    }
    let mut processes = None;
    let mut pool_max = None;
    let mut generations = None;
    let mut workers = None;
    let mut headroom = None;
    for pair in rest.chunks(2) {
        if pair.len() != 2 {
            return Err(CliError::new(Exit::Usage, USAGE));
        }
        let slot = match pair[0].as_str() {
            "--processes-per-generation" => &mut processes,
            "--pool-max" => &mut pool_max,
            "--generations" => &mut generations,
            "--workers" => &mut workers,
            "--admin-headroom" => &mut headroom,
            _ => return Err(CliError::new(Exit::Usage, USAGE)),
        };
        if slot.is_some() {
            return Err(CliError::new(Exit::Usage, USAGE));
        }
        *slot = Some(
            pair[1]
                .parse::<u32>()
                .map_err(|_| CliError::new(Exit::Usage, USAGE))?,
        );
    }
    let required = |value: Option<u32>| {
        value.ok_or_else(|| CliError::new(Exit::Usage, "rolling preflight requires processes-per-generation, pool-max, generations, workers and admin-headroom"))
    };
    let budget = PostgresConnectionBudget {
        processes_per_generation: required(processes)?,
        pool_max: required(pool_max)?,
        generations: required(generations)?,
        workers: required(workers)?,
        admin_headroom: required(headroom)?,
    };
    budget
        .peak_connections()
        .map_err(|error| CliError::new(Exit::Usage, error.to_string()))?;
    Ok(Some(budget))
}

/// `preflight`'s result body: the facade's typed schema report, with the
/// PostgreSQL-only connection-budget answer appended when the caller asked
/// for one.
///
/// The schema fields serialize from [`lash::preflight::SchemaReport`], the
/// same projection `probe_store` reports: one conversion from
/// `StoreSchemaStatus` exists, so this wire cannot drift from the facade's.
#[derive(Serialize)]
struct PreflightJson<'a> {
    #[serde(flatten)]
    schema: lash::preflight::SchemaReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    connection_budget: Option<&'a PostgresConnectionBudgetReport>,
}

fn preflight_result(
    status: &StoreSchemaStatus,
    connection_budget: Option<&PostgresConnectionBudgetReport>,
) -> Result<Value, CliError> {
    serde_json::to_value(PreflightJson {
        schema: lash::preflight::schema_report(status),
        connection_budget,
    })
    .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))
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
        ObligationKey::ScopeClose { session_id, run } => {
            json!({"session_id":session_id.as_str(),"root":run.as_str()})
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

fn version_result() -> Value {
    json!({
        "release": env!("CARGO_PKG_VERSION"),
        "fleet_writable": FLEET_WRITABLE_RANGE,
        "components": DESCRIPTORS.iter().map(|descriptor| json!({
            "component": descriptor.component.as_str(),
            "reads": descriptor.reads,
            "writes": descriptor.writes,
        })).collect::<Vec<_>>(),
        "wires": {
            "remote_protocol": lash_remote_protocol::REMOTE_PROTOCOL,
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
    let outcome = match command {
        Command::Recovery(invocation) => (invocation.run().await?, Exit::Done),
        Command::Version => (version_result(), Exit::Done),
        Command::Migrate { phase, dry_run } => {
            let url = database_url()?;
            let report = if *dry_run {
                PostgresStorage::plan_migrations(&url, *phase).await
            } else {
                PostgresStorage::migrate(&url, *phase).await
            }
            .map_err(CliError::migrate)?;
            (migration_result(&report, *dry_run), Exit::Done)
        }
        Command::Preflight { budget } => {
            let url = database_url()?;
            let probe = PostgresStorePreflight::for_database_url(&url).map_err(CliError::store)?;
            let capacity = if let Some(budget) = budget {
                let checked = probe
                    .connection_capacity()
                    .await
                    .map_err(CliError::store)
                    .and_then(|capacity| {
                        budget.check(capacity).map_err(|refusal| {
                            CliError::refused(Exit::Refused, refusal.to_string(), &refusal)
                        })
                    });
                match checked {
                    Ok(report) => Some(report),
                    Err(error) => {
                        probe.close().await;
                        return Err(error);
                    }
                }
            } else {
                None
            };
            let status = probe.schema_status().await.map_err(CliError::store);
            probe.close().await;
            let status = status?;
            let exit = match status.outcome() {
                StoreSchemaOutcome::Ready => Exit::Done,
                StoreSchemaOutcome::Refused => Exit::Incompatible,
                StoreSchemaOutcome::Undecided => Exit::Refused,
                _ => Exit::Refused,
            };
            (preflight_result(&status, capacity.as_ref())?, exit)
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

    /// FIG-5037: recovery commands require an exact park token; paging cursors
    /// and delivery kinds are validated before opening any operator backend.
    #[test]
    fn recovery_verbs_validate_tokens_cursors_and_delivery_kinds() {
        let process = json!({"kind":"process", "process_id":lash::ProcessId::fixture("recovery")})
            .to_string();
        let valid = [
            vec!["park", "list", "--limit", "1"],
            vec!["park", "events", "--after", r#"{"turn":0,"process":0}"#],
            vec![
                "park",
                "redrive",
                "--target",
                process.as_str(),
                "--park-id",
                "1",
            ],
            vec![
                "park",
                "cancel",
                "--target",
                r#"{"kind":"turn","session_id":"s","turn_id":"t"}"#,
                "--park-id",
                "1",
            ],
            vec![
                "park",
                "fork",
                "--target",
                r#"{"kind":"turn","session_id":"s","turn_id":"t"}"#,
                "--park-id",
                "1",
            ],
            vec![
                "stalled", "list", "ingress", "--after", "delivery", "--limit", "1",
            ],
            vec!["stalled", "rearm", "ingress", "delivery"],
            vec!["deployment-status", "--accepting-new-work", "false"],
        ];
        for words in valid {
            assert!(
                parse(words.iter().map(|word| (*word).to_owned())).is_ok(),
                "{words:?}"
            );
        }
        for words in [
            vec!["park", "redrive", "--target", process.as_str()],
            vec!["park", "list", "--limit", "0"],
            vec!["park", "list", "--after", "broken"],
            vec!["stalled", "list", "unknown"],
            vec!["stalled", "rearm", "ingress"],
            vec!["deployment-status"],
        ] {
            let error = parse(words.iter().map(|word| (*word).to_owned()))
                .err()
                .expect("invalid recovery arguments");
            assert_eq!(error.exit as u8, Exit::Usage as u8, "{words:?}");
        }
    }

    #[test]
    fn release_inventory_build_probe() {
        let formats: Vec<_> = lash::formats::durable_formats()
            .map(|entry| {
                let value = match entry.version {
                    lash::formats::FormatVersion::Counter(value) => json!(value),
                    lash::formats::FormatVersion::Identity(value) => json!(value),
                    other => panic!("unhandled format version: {other:?}"),
                };
                json!({"constant": entry.constant, "value": value})
            })
            .collect();
        assert!(!formats.is_empty(), "the build exposes its durable formats");
        println!(
            "release-inventory-build={}",
            json!({
                "version": version_result(), "formats": formats,
            })
        );
    }

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
    fn rolling_preflight_requires_every_budget_term_and_refuses_excess_capacity() {
        let args = words(&[
            "preflight",
            "--processes-per-generation",
            "2",
            "--pool-max",
            "18",
            "--generations",
            "3",
            "--workers",
            "12",
            "--admin-headroom",
            "10",
        ]);
        let invocation = parse(args.clone()).unwrap_or_else(|error| panic!("{}", error.message));
        let Command::Preflight {
            budget: Some(budget),
        } = invocation.command
        else {
            panic!("roll budget missing");
        };
        assert!(
            matches!(budget.check(lash_postgres_store::PostgresConnectionCapacity { max_connections: 100, reserved_connections: 3 }), Err(lash_postgres_store::PostgresConnectionBudgetRefusal::ConnectionBudgetExceeded { report }) if report.peak_connections == 130)
        );
        for index in (1..args.len()).step_by(2) {
            let mut missing = args.clone();
            missing.drain(index..index + 2);
            assert!(parse(missing).is_err(), "missing budget term at {index}");
        }
        for bad in ["0", "-1", "4294967296", "nonsense"] {
            let mut invalid = args.clone();
            invalid[2] = bad.to_owned();
            assert!(parse(invalid).is_err());
        }
        let mut duplicate = args;
        duplicate.extend(words(&["--workers", "12"]));
        assert!(parse(duplicate).is_err());
    }

    #[test]
    fn a_stalled_obligation_lists_its_identity_and_typed_reason() {
        use lash_core_store::store::{
            KeyColumn, ObligationId, ObligationKind, StallReason, UndecodableObligation,
        };

        let decoded = StalledObligation {
            kind: ObligationKind::ControlIntent,
            id: ObligationId::new("obligation-decoded"),
            key: ObligationKey::decode(ObligationKind::ControlIntent, vec![KeyColumn::Integer(7)]),
            reason: StallReason::AttemptsExhausted,
            attempts: 3,
            last_error: Some(lash_core_store::store::DeliveryError::new(
                lash_core_execution::RuntimeErrorCode::EngineControlRequest,
                "the engine was unavailable",
            )),
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
                "last_error": {
                    "code": "engine_control_request",
                    "message": "the engine was unavailable",
                },
                "stalled_at_ms": 11,
            })
        );

        let foreign = StalledObligation {
            kind: ObligationKind::ArtifactCleanup,
            id: ObligationId::new("obligation-foreign"),
            key: Err(UndecodableObligation {
                code: lash_core_execution::RuntimeErrorCode::StoreIncompatible,
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
