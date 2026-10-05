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
use lash_core_store::store::plugin_writers::PluginWriterRegistration;
use lash_core_store::store::{
    FLEET_WRITABLE_RANGE, StorePreflight, StoreSchemaOutcome, StoreSchemaStatus,
};
use lash_core_store::store::{ObligationKey, ObligationKind, StalledObligation, StoreError};
use lash_postgres_store::{
    FinalizeReport, MigrateError, MigrationPhase, MigrationReport, MigrationStep,
    PostgresConnectionBudget, PostgresConnectionBudgetReport, PostgresStorage, PostgresStoreConfig,
    PostgresStorePreflight,
};
mod recovery;

use serde::Serialize;
use serde_json::{Value, json};

/// version_guard(
///     shapes(cover(StepDto, PreflightJson)),
///     roots(Exit, CliError),
///     roots(path = "crates/lash-core-store/src/store/fleet_finalize.rs", FinalizeRefusal),
///     roots(path = "crates/lash-postgres-store/src/postgres/migrate.rs", MigrationRefusal),
///     roots(path = "crates/lash-restate/src/object_upgrade.rs", ObjectUpgradeError),
///     items(
///         name, from, run, output, error_json, objects_preflight_result, objects_sweep_result,
///         finalize_result, hold_result, migration_result, stalled_row, stalled_result,
///         drain_status_result, version_result, preflight_result,
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
/// The most stalled obligations `drain-status` lists per kind, first by id;
/// `stalled_obligations` still counts every one.
const STALLED_LISTED_PER_KIND: std::num::NonZeroUsize = std::num::NonZeroUsize::new(100).unwrap();
const USAGE: &str = "usage: lashctl [--json] <migrate [--phase expand|backfill|contract] [--dry-run] | drain <generation> | drain-status <generation> --restate-admin-url <url> | end-drain <generation> | finalize <retired-generation> --restate-admin-url <url> [--override-hold] [--plugin-registrations <json-file>] | finalize-hold show | finalize-hold set --reason <text> | finalize-hold clear | objects-preflight --restate-admin-url <url> [--namespace <ns>] | objects-sweep --restate-admin-url <url> --restate-ingress-url <url> [--namespace <ns>] | preflight [--processes-per-generation <n> --pool-max <n> --generations <n> --workers <n> --admin-headroom <n>] | park list|events [--after <json>] [--limit <n>] | park redrive|cancel|fork --target <json> --park-id <n> | stalled list <kind> [--after <id>] [--limit <n>] | stalled rearm <kind> <id> | deployment-status --accepting-new-work <bool> (recovery commands accept --sqlite-dir <path>) | version>";

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

/// Read the successor deployment's registrar-derived writer declarations.
fn parse_plugin_registrations(bytes: &[u8]) -> Result<Vec<PluginWriterRegistration>, CliError> {
    let registrations: Vec<PluginWriterRegistration> = serde_json::from_slice(bytes)
        .map_err(|error| CliError::new(Exit::Usage, format!("plugin registrations: {error}")))?;
    let mut plugins = std::collections::BTreeSet::new();
    for registration in &registrations {
        let writable: std::collections::BTreeSet<_> = registration.writable.iter().collect();
        if registration.plugin.is_empty()
            || !plugins.insert(&registration.plugin)
            || !writable.contains(&registration.native)
            || writable.len() != registration.writable.len()
        {
            return Err(CliError::new(
                Exit::Usage,
                format!(
                    "invalid or duplicate plugin registration: {}",
                    registration.plugin
                ),
            ));
        }
    }
    Ok(registrations)
}

enum Command {
    Recovery(recovery::Invocation),
    Migrate {
        phase: MigrationPhase,
        dry_run: bool,
    },
    Drain {
        generation: BuildGeneration,
    },
    DrainStatus {
        generation: BuildGeneration,
        /// The engine's admin API: the unfinished invocations still pinned
        /// to the generation's deployments are read there (FIG-4454).
        restate_admin_url: String,
    },
    EndDrain {
        generation: BuildGeneration,
    },
    Finalize {
        retired: BuildGeneration,
        restate_admin_url: String,
        mode: FinalizeMode,
        plugin_registrations: Option<std::path::PathBuf>,
    },
    FinalizeHold(HoldAction),
    ObjectsPreflight {
        restate: RestateTarget,
    },
    ObjectsSweep {
        restate: RestateTarget,
    },
    Preflight {
        budget: Option<PostgresConnectionBudget>,
    },
    Version,
}

/// The Restate server an object command reads, and calls through for a
/// sweep, in one namespace.
struct RestateTarget {
    admin_url: String,
    ingress_url: Option<String>,
    namespace: lash_restate::RestateNamespace,
}

enum HoldAction {
    Show,
    Set { reason: String },
    Clear,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Recovery(invocation) => invocation.command.name(),
            Self::Migrate { .. } => "migrate",
            Self::Drain { .. } => "drain",
            Self::DrainStatus { .. } => "drain-status",
            Self::EndDrain { .. } => "end-drain",
            Self::Finalize { .. } => "finalize",
            Self::FinalizeHold(_) => "finalize-hold",
            Self::ObjectsPreflight { .. } => "objects-preflight",
            Self::ObjectsSweep { .. } => "objects-sweep",
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
        "drain" | "end-drain" if rest.len() == 1 => {
            let generation = BuildGeneration::parse(&rest[0])
                .map_err(|_| CliError::new(Exit::Usage, "invalid build generation"))?;
            match verb {
                "drain" => Command::Drain { generation },
                _ => Command::EndDrain { generation },
            }
        }
        "drain-status" if !rest.is_empty() => {
            let generation = BuildGeneration::parse(&rest[0])
                .map_err(|_| CliError::new(Exit::Usage, "invalid build generation"))?;
            let restate_admin_url = match &rest[1..] {
                [flag, url] if flag == "--restate-admin-url" => url.clone(),
                [] => {
                    return Err(CliError::new(
                        Exit::Usage,
                        "drain-status needs --restate-admin-url: undrained group children are read from the engine",
                    ));
                }
                _ => return Err(CliError::new(Exit::Usage, USAGE)),
            };
            Command::DrainStatus {
                generation,
                restate_admin_url,
            }
        }
        "finalize" if !rest.is_empty() => {
            let retired = BuildGeneration::parse(&rest[0])
                .map_err(|_| CliError::new(Exit::Usage, "invalid build generation"))?;
            let mut restate_admin_url = None;
            let mut mode = FinalizeMode::Automatic;
            let mut plugin_registrations = None;
            let mut index = 1;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--restate-admin-url"
                        if index + 1 < rest.len() && restate_admin_url.is_none() =>
                    {
                        restate_admin_url = Some(rest[index + 1].clone());
                        index += 2;
                    }
                    "--plugin-registrations"
                        if index + 1 < rest.len() && plugin_registrations.is_none() =>
                    {
                        plugin_registrations = Some(std::path::PathBuf::from(&rest[index + 1]));
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
                plugin_registrations,
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
        "objects-preflight" | "objects-sweep" => {
            let restate = parse_restate_target(rest, verb == "objects-sweep")?;
            if verb == "objects-sweep" {
                Command::ObjectsSweep { restate }
            } else {
                Command::ObjectsPreflight { restate }
            }
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

/// `--restate-admin-url <url> [--restate-ingress-url <url>] [--namespace <ns>]`;
/// a sweep calls `upgrade` through ingress, so it needs the ingress URL.
fn parse_restate_target(rest: &[String], sweep: bool) -> Result<RestateTarget, CliError> {
    let mut admin_url = None;
    let mut ingress_url = None;
    let mut namespace = None;
    let mut index = 0;
    while index + 1 < rest.len() {
        let value = rest[index + 1].clone();
        let slot = match rest[index].as_str() {
            "--restate-admin-url" => &mut admin_url,
            "--restate-ingress-url" if sweep => &mut ingress_url,
            "--namespace" => &mut namespace,
            _ => return Err(CliError::new(Exit::Usage, USAGE)),
        };
        if slot.replace(value).is_some() {
            return Err(CliError::new(Exit::Usage, USAGE));
        }
        index += 2;
    }
    if index != rest.len() {
        return Err(CliError::new(Exit::Usage, USAGE));
    }
    let admin_url = admin_url.ok_or_else(|| {
        CliError::new(
            Exit::Usage,
            "object commands need --restate-admin-url: objects are read from the engine's state",
        )
    })?;
    if sweep && ingress_url.is_none() {
        return Err(CliError::new(
            Exit::Usage,
            "objects-sweep needs --restate-ingress-url: each object's `upgrade` handler is called there",
        ));
    }
    let namespace = lash_restate::RestateNamespace::new(namespace.unwrap_or_default())
        .map_err(|error| CliError::new(Exit::Usage, format!("--namespace: {error}")))?;
    Ok(RestateTarget {
        admin_url,
        ingress_url,
        namespace,
    })
}

impl RestateTarget {
    fn target(&self) -> lash_restate::RestateObjectUpgradeTarget {
        let admin = lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(
            self.admin_url.clone(),
        ));
        match &self.ingress_url {
            Some(ingress_url) => lash_restate::RestateObjectUpgradeTarget::new(
                admin,
                lash_restate::RestateIngressClient::new(lash_restate::RestateConnection::new(
                    ingress_url.clone(),
                )),
                self.namespace.clone(),
            ),
            None => {
                lash_restate::RestateObjectUpgradeTarget::read_only(admin, self.namespace.clone())
            }
        }
    }
}

impl CliError {
    /// Before finalize the sweep is a refused precondition, exit 3; an
    /// object whose `_compat` refuses this build is an incompatible store,
    /// exit 4; an engine that cannot be read or called fails, exit 1.
    fn objects(error: lash_restate::ObjectUpgradeError) -> Self {
        let exit = match &error {
            lash_restate::ObjectUpgradeError::NotFinalized { .. } => Exit::Refused,
            lash_restate::ObjectUpgradeError::Incompatible { .. } => Exit::Incompatible,
            lash_restate::ObjectUpgradeError::Engine { .. } => Exit::Unexpected,
        };
        match &error {
            lash_restate::ObjectUpgradeError::Engine { .. } => Self::new(exit, error.to_string()),
            _ => Self::refused(exit, error.to_string(), &error),
        }
    }
}

fn objects_preflight_result(preflight: &lash_restate::ObjectPreflight) -> Value {
    json!({
        "upgraded": preflight.upgraded(),
        "families": preflight.families.iter().map(|family| json!({
            "service": family.service,
            "component": family.component,
            "newest": family.newest,
            "objects": family.objects,
            "pending": family.pending.iter().map(|pending| json!({
                "key": pending.key,
                "format": pending.format,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn objects_sweep_result(report: &lash_restate::SweepReport) -> Value {
    json!({
        "swept": report.swept.iter().map(|object| json!({
            "service": object.service,
            "key": object.key,
            "outcome": object.outcome,
        })).collect::<Vec<_>>(),
        "remaining": report.remaining.iter().map(|pending| json!({
            "service": pending.service,
            "key": pending.key,
            "format": pending.format,
        })).collect::<Vec<_>>(),
    })
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
    let outcome = match command {
        Command::Recovery(invocation) => (invocation.run().await?, Exit::Done),
        Command::ObjectsPreflight { restate } => {
            let preflight = lash_restate::preflight_objects(&restate.target())
                .await
                .map_err(CliError::objects)?;
            let exit = if preflight.upgraded() {
                Exit::Done
            } else {
                Exit::NotYet
            };
            (objects_preflight_result(&preflight), exit)
        }
        Command::ObjectsSweep { restate } => {
            let report = lash_restate::sweep_objects(&restate.target(), |_| {})
                .await
                .map_err(CliError::objects)?;
            let exit = if report.remaining.is_empty() {
                Exit::Done
            } else {
                Exit::NotYet
            };
            (objects_sweep_result(&report), exit)
        }
        Command::Version => {
            let storage = PostgresStorage::connect_with(
                &database_url()?,
                PostgresStoreConfig {
                    max_connections: OPERATOR_POOL_MAX,
                    ..Default::default()
                },
            )
            .await
            .map_err(CliError::store)?;
            let generations = storage.fleet_generations().await.map_err(CliError::store)?;
            (version_result(&generations), Exit::Done)
        }
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
        Command::Finalize {
            retired,
            restate_admin_url,
            mode,
            plugin_registrations,
        } => {
            let registrations = plugin_registrations
                .as_ref()
                .map(|path| {
                    let bytes = std::fs::read(path).map_err(|error| {
                        CliError::new(Exit::Usage, format!("read {}: {error}", path.display()))
                    })?;
                    parse_plugin_registrations(&bytes)
                })
                .transpose()?
                .unwrap_or_default();
            let storage = PostgresStorage::connect_with(
                &database_url()?,
                PostgresStoreConfig {
                    max_connections: OPERATOR_POOL_MAX,
                    ..Default::default()
                },
            )
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
                    &registrations,
                    lash_core_execution::facade_support::SystemClock.timestamp_ms(),
                )
                .await
                .map_err(CliError::finalize)?;
            (finalize_result(&report), Exit::Done)
        }
        Command::FinalizeHold(action) => {
            let storage = PostgresStorage::connect_with(
                &database_url()?,
                PostgresStoreConfig {
                    max_connections: OPERATOR_POOL_MAX,
                    ..Default::default()
                },
            )
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
        Command::Drain { generation }
        | Command::EndDrain { generation }
        | Command::DrainStatus { generation, .. } => {
            let storage = PostgresStorage::connect_with(
                &database_url()?,
                PostgresStoreConfig {
                    max_connections: OPERATOR_POOL_MAX,
                    ..Default::default()
                },
            )
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
                Command::DrainStatus {
                    restate_admin_url, ..
                } => {
                    let registry = lash_restate::RestateDeploymentRegistry::new(
                        lash_restate::RestateAdminClient::new(
                            lash_restate::RestateConnection::new(restate_admin_url.clone()),
                        ),
                    );
                    let status = GenerationDrainStatus::collect(
                        drain.as_ref(),
                        storage.session_delete_ledger().as_ref(),
                        |kind| storage.obligation_ledger(kind),
                        &registry,
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
                        Exit::NotYet => Some(CliError::new(
                            status,
                            match invocation.command {
                                Command::ObjectsPreflight { .. } | Command::ObjectsSweep { .. } => {
                                    "objects remain at an older family format"
                                }
                                _ => "the generation is not yet drained",
                            },
                        )),
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
                "version": version_result(&[]), "formats": formats,
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
    fn finalize_rejects_invalid_successor_registrations_before_opening_storage() {
        for input in [
            r#"[{"plugin":"counter","native":0,"writable":[1]}]"#,
            r#"[{"plugin":"counter","native":2,"writable":[1]}]"#,
            r#"[{"plugin":"counter","native":1,"writable":[1,1]}]"#,
            r#"[{"plugin":"","native":1,"writable":[1]}]"#,
            r#"[{"plugin":"counter","native":1,"writable":[1]},{"plugin":"counter","native":2,"writable":[1,2]}]"#,
        ] {
            let error =
                parse_plugin_registrations(input.as_bytes()).expect_err("refused registration");
            assert_eq!(error.exit as u8, Exit::Usage as u8);
        }
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
                ..
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

    #[test]
    fn a_stalled_obligation_lists_its_identity_and_typed_reason() {
        use lash_core_store::store::{KeyColumn, ObligationId, StallReason, UndecodableObligation};

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
