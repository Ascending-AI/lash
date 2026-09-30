//! The FIG-3790 durable load workload (lane L3, FIG-4168).
//!
//! A driver replays a checked-in synthetic workload (`lash_perf::workload`)
//! through lash's public API. Every turn, queued input, cancel, host process,
//! cron emission and session delete is one [`LoadRequest`] that a worker's
//! [`E2eLoadWorkflow`](worker::E2eLoadWorkflow) runs inside its Restate
//! handler, over the worker's one `LashCore`. The scripted provider answers
//! each turn with the RLM cell the workload generates for it, and that cell
//! calls the three synthetic tools in [`tools`].
//!
//! Evidence is independent of lash's own store: the driver witnesses what it
//! sent and the typed terminal it read back, the provider receipts every
//! completion it served, the synthetic tools offer every effect to the
//! witness's idempotent receiver, and the attachment tool and the workers'
//! read endpoint witness the exact blob bytes they put and read. [`verify`]
//! reconciles all of it against the plan the workload regenerates.

pub mod tools;
pub mod verify;
pub mod worker;

use anyhow::{Context, Result, ensure};
use lash_perf::workload::{Generator, Workload};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

/// The Restate workflow every load operation runs as.
pub const LOAD_WORKFLOW: &str = "E2eLoadWorkflow";
/// Names the checked-in workload every process of a run generates from.
pub const LOAD_WORKLOAD_ENV: &str = "LASH_LOAD_WORKLOAD";
/// The trigger source a cron emission publishes on.
pub const CRON_SOURCE_TYPE: &str = "load.cron.tick";
/// The typed event a cron emission carries.
pub const CRON_EVENT_TYPE: &str = "load.cron.Tick";

/// Provider markers. A load turn's input names its operation key, and the
/// provider regenerates the cell for that key from the same workload.
pub const TURN_MARKER: &str = "load_turn=";
pub const QUEUED_MARKER: &str = "load_queued=";
pub const CRON_SETUP_MARKER: &str = "load_cron_setup=";
pub const WORKLOAD_MARKER: &str = "load_workload=";

/// The workload a load process was started with, validated at startup.
#[derive(Clone, Debug)]
pub struct LoadContext {
    pub workload_name: String,
    pub workload: Workload,
}

impl LoadContext {
    /// The workload [`LOAD_WORKLOAD_ENV`] names, or `None` when the process
    /// serves no load run (the ordinary e2e harness).
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(LOAD_WORKLOAD_ENV) {
            Ok(name) => Ok(Some(Self::named(&name)?)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {LOAD_WORKLOAD_ENV}")),
        }
    }

    pub fn named(name: &str) -> Result<Self> {
        Ok(Self {
            workload_name: name.to_owned(),
            workload: Workload::named(name)?,
        })
    }

    pub fn sha256(&self) -> &str {
        self.workload.sha256()
    }

    /// Refuse an operation generated from a different workload.
    pub fn require_workload(&self, sha256: &str) -> Result<()> {
        ensure!(
            sha256 == self.sha256(),
            "the operation was generated from workload {sha256}, this process runs {} ({})",
            self.workload_name,
            self.sha256()
        );
        Ok(())
    }

    pub fn generator(&self, run: &str) -> Result<Generator<'_>> {
        Generator::new(&self.workload, run)
    }
}

/// One load operation, as the driver submits it to [`LOAD_WORKFLOW`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LoadRequest {
    /// Primary turn `ordinal` of `actor`, with its queued inputs, cancels and
    /// host process starts, in session `session_id`.
    Turn {
        workload_sha256: String,
        run: String,
        actor: u64,
        ordinal: u64,
        session_id: String,
    },
    /// The turn that registers every cron schedule of the run.
    CronSetup {
        workload_sha256: String,
        run: String,
        session_id: String,
    },
    /// One scheduled emission of cron schedule `subscription`.
    CronTick {
        workload_sha256: String,
        run: String,
        subscription: u64,
        tick: u64,
    },
    /// Delete a session the run retired (rotation or a planned delete).
    DeleteSession { run: String, session_id: String },
}

impl LoadRequest {
    /// The Restate workflow key: one invocation per operation, so a retried
    /// submission attaches to the same invocation.
    pub fn workflow_key(&self) -> String {
        match self {
            Self::Turn {
                run,
                actor,
                ordinal,
                ..
            } => format!("load-{run}-turn-{actor}-{ordinal}"),
            Self::CronSetup { run, .. } => format!("load-{run}-cron-setup"),
            Self::CronTick {
                run,
                subscription,
                tick,
                ..
            } => format!("load-{run}-cron-{subscription}-tick-{tick}"),
            Self::DeleteSession { run, session_id } => format!("load-{run}-delete-{session_id}"),
        }
    }

    pub fn run(&self) -> &str {
        match self {
            Self::Turn { run, .. }
            | Self::CronSetup { run, .. }
            | Self::CronTick { run, .. }
            | Self::DeleteSession { run, .. } => run,
        }
    }
}

/// The session actor `actor` uses for its `generation`th session of `run`.
pub fn actor_session_id(run: &str, actor: u64, generation: u64) -> String {
    format!("load-{run}-{actor}-g{generation}")
}

/// The session that owns the run's cron subscriptions.
pub fn cron_session_id(run: &str) -> String {
    format!("load-{run}-cron")
}

/// A lash turn id derived from an operation key: keys contain `/`, turn ids
/// are a single path segment.
pub fn turn_id_for(key: &str) -> String {
    format!("load-{}", key.replace('/', "-"))
}

/// How a root that took an input stood once it stopped moving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedStatus {
    Answered,
    Failed,
    Cancelled,
    Parked,
    Stalled,
    /// A status variant this harness does not know yet.
    Unrecognized,
}

impl From<&lash::TurnStatus> for ReportedStatus {
    fn from(status: &lash::TurnStatus) -> Self {
        match status {
            lash::TurnStatus::Answered => Self::Answered,
            lash::TurnStatus::Failed => Self::Failed,
            lash::TurnStatus::Cancelled => Self::Cancelled,
            lash::TurnStatus::Parked(_) => Self::Parked,
            lash::TurnStatus::Stalled(_) => Self::Stalled,
            _ => Self::Unrecognized,
        }
    }
}

/// An accepted input's terminal, as `SendOutcome` reported it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputOutcome {
    pub status: ReportedStatus,
    pub root: Option<String>,
    /// The cell's `finish` value; `null` when the root ran no finishing cell.
    pub final_value: Value,
    /// The settled turn's typed outcome, for a failure's diagnosis.
    pub outcome: Value,
}

impl InputOutcome {
    /// The operation key the answering cell finished with.
    pub fn finished_operation(&self) -> Option<&str> {
        self.final_value.get("operation").and_then(Value::as_str)
    }
}

/// What a cancel did, as lash's `CancelReceipt` reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    Withdrawn,
    Requested,
    AlreadySettled,
    NotFound,
    /// A receipt variant this harness does not know yet.
    Unrecognized,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueuedReport {
    pub key: String,
    pub during_active_turn: bool,
    pub cancel: Option<CancelOutcome>,
    pub outcome: InputOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostProcessReport {
    pub key: String,
    pub process_id: String,
    pub created: bool,
    pub signalled: bool,
    pub cancel_requested: bool,
    /// The awaited terminal: the body's value, or the cancellation it ended with.
    pub output: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnReport {
    pub worker_id: String,
    pub operation: String,
    pub session_id: String,
    pub outcome: InputOutcome,
    pub cancel: Option<CancelOutcome>,
    pub queued: Vec<QueuedReport>,
    pub host_processes: Vec<HostProcessReport>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CronSetupReport {
    pub worker_id: String,
    pub outcome: InputOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CronTickReport {
    pub worker_id: String,
    pub schedule: String,
    pub key: String,
    pub started_process_ids: Vec<String>,
    pub outputs: Vec<Value>,
}

/// How a delete ended, as lash's `SessionDeletion` reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionOutcome {
    Deleted,
    AlreadyDeleted,
    Closing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteReport {
    pub worker_id: String,
    pub session_id: String,
    pub deletion: DeletionOutcome,
    /// The typed refusal a fresh open of the deleted session met; `None`
    /// would mean the session still opened.
    pub reopen_refusal: Option<String>,
    /// How long after the delete the refusal was first seen: a closing
    /// session is refused once the recovery relay finished its delete.
    pub refused_after_ms: u64,
    /// What a closing deletion was still waiting on, as lash reported it.
    pub closing_waits: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LoadResponse {
    Turn(TurnReport),
    CronSetup(CronSetupReport),
    CronTick(CronTickReport),
    DeleteSession(DeleteReport),
}

/// The operation a witness row describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WitnessedOperation {
    Turn,
    DeleteSession,
    CronSetup,
    CronTick,
    Attachment,
}

impl WitnessedOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::DeleteSession => "delete-session",
            Self::CronSetup => "cron-setup",
            Self::CronTick => "cron-tick",
            Self::Attachment => "attachment",
        }
    }

    pub fn of(request: &LoadRequest) -> Self {
        match request {
            LoadRequest::Turn { .. } => Self::Turn,
            LoadRequest::CronSetup { .. } => Self::CronSetup,
            LoadRequest::CronTick { .. } => Self::CronTick,
            LoadRequest::DeleteSession { .. } => Self::DeleteSession,
        }
    }
}

/// The phase of an operation a witness row records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WitnessedPhase {
    Sent,
    Terminal,
    Put,
    Read,
}

impl WitnessedPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Terminal => "terminal",
            Self::Put => "put",
            Self::Read => "read",
        }
    }
}

/// One `witness_load_events` row.
pub struct LoadEvent<'a> {
    pub run: &'a str,
    pub subject: &'a str,
    pub operation: WitnessedOperation,
    pub phase: WitnessedPhase,
    pub observer: &'a str,
    pub detail: &'a Value,
    /// Exact bytes the observer handled; the witness digests them.
    pub content: Option<&'a [u8]>,
}

pub async fn record_load_event(pool: &PgPool, event: LoadEvent<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO witness_load_events (
             run_id, subject, operation, phase, observer, detail_json, content_bytes
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(event.run)
    .bind(event.subject)
    .bind(event.operation.as_str())
    .bind(event.phase.as_str())
    .bind(event.observer)
    .bind(event.detail.to_string())
    .bind(event.content)
    .execute(pool)
    .await
    .with_context(|| {
        format!(
            "witness {} {} of `{}`",
            event.operation.as_str(),
            event.phase.as_str(),
            event.subject
        )
    })?;
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "`load.cron.Tick` and its string fields satisfy lash::rlm::NamedDataType::object's validation"
)]
fn load_cron_tick_event_type() -> lash::rlm::NamedDataType {
    lash::rlm::NamedDataType::object(
        CRON_EVENT_TYPE,
        vec![
            lash::rlm::TypeField {
                name: "schedule".into(),
                ty: lash::rlm::TypeExpr::Str,
                optional: false,
            },
            lash::rlm::TypeField {
                name: "tick".into(),
                ty: lash::rlm::TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .expect("valid load cron payload type")
}

fn load_cron_tick_payload_schema() -> lash::triggers::LashSchema {
    lash::triggers::LashSchema::new(serde_json::json!({
        "type": "object",
        "properties": {
            "schedule": { "type": "string" },
            "tick": { "type": "string" }
        },
        "required": ["schedule", "tick"],
        "additionalProperties": false
    }))
}

/// Declare the `load.cron.tick({schedule})` trigger source cells register on.
#[expect(
    clippy::expect_used,
    reason = "the load cron source's name and parameter type satisfy the catalog grammar"
)]
pub(crate) fn register_cron_trigger_source(catalog: &mut lash::rlm::LashlangHostCatalog) {
    catalog
        .add_trigger_source_constructor(
            ["load", "cron", "tick"],
            lash::rlm::TypeExpr::Object(vec![lash::rlm::TypeField {
                name: "schedule".into(),
                ty: lash::rlm::TypeExpr::Str,
                optional: false,
            }]),
            load_cron_tick_event_type(),
        )
        .expect("valid load cron trigger source");
}

/// The session-side declaration of the cron emission event.
pub(crate) fn cron_trigger_event() -> lash::triggers::TriggerEvent {
    lash::triggers::TriggerEvent::new("Tick", "load.cron", "tick", load_cron_tick_payload_schema())
}

/// A tool argument that must be a whole number. Cell numbers arrive as
/// JSON numbers that may carry a fractional representation.
pub(crate) fn whole_number(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| *number >= 0.0 && number.fract() == 0.0 && *number <= 2f64.powi(53))
            .map(|number| number as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_keys_are_unique_per_operation_and_path_safe() {
        let requests = [
            LoadRequest::Turn {
                workload_sha256: "w".into(),
                run: "r".into(),
                actor: 1,
                ordinal: 2,
                session_id: actor_session_id("r", 1, 0),
            },
            LoadRequest::CronSetup {
                workload_sha256: "w".into(),
                run: "r".into(),
                session_id: cron_session_id("r"),
            },
            LoadRequest::CronTick {
                workload_sha256: "w".into(),
                run: "r".into(),
                subscription: 1,
                tick: 2,
            },
            LoadRequest::DeleteSession {
                run: "r".into(),
                session_id: actor_session_id("r", 1, 0),
            },
        ];
        let keys: std::collections::BTreeSet<_> =
            requests.iter().map(LoadRequest::workflow_key).collect();
        assert_eq!(keys.len(), requests.len());
        assert!(keys.iter().all(|key| !key.contains('/')));
        assert_eq!(turn_id_for("r/1/2/queued/0"), "load-r-1-2-queued-0");
        for request in requests {
            let encoded = serde_json::to_string(&request).expect("encode request");
            let decoded: LoadRequest = serde_json::from_str(&encoded).expect("decode request");
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn whole_numbers_accept_integral_floats_only() {
        assert_eq!(whole_number(Some(&serde_json::json!(1024))), Some(1024));
        assert_eq!(whole_number(Some(&serde_json::json!(1024.0))), Some(1024));
        assert_eq!(whole_number(Some(&serde_json::json!(1.5))), None);
        assert_eq!(whole_number(Some(&serde_json::json!(-1))), None);
        assert_eq!(whole_number(Some(&serde_json::json!("1"))), None);
        assert_eq!(whole_number(None), None);
    }

    #[test]
    fn workloads_resolve_by_name_and_refuse_a_different_digest() {
        let smoke = LoadContext::named("smoke-v1").expect("smoke workload");
        let figments = LoadContext::named("figments-v1").expect("figments workload");
        assert!(smoke.require_workload(smoke.sha256()).is_ok());
        assert!(smoke.require_workload(figments.sha256()).is_err());
        assert!(LoadContext::named("unknown").is_err());
    }
}
