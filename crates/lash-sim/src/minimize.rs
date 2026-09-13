use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::generator::generate_workload;
use crate::oracles::{
    LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE, combine_oracles, generated_trace_oracles,
};
use crate::replay::{ReplayError, replay_trace};
use crate::runner::run_generated_workload_for_fixture;
use crate::scheduler::BoundaryKind;
use crate::store::ModelStore;
use crate::trace::{
    AbstractWorldSummary, OracleStatus, OracleVerdict, SimulationTrace, TraceIoError, read_trace,
    write_replay_report, write_trace,
};

pub const MINIMIZE_REPORT_SCHEMA: &str = "lash.sim.minimize-report.v1";
pub const FAILURE_PACKAGE_SCHEMA: &str = "lash.sim.failure-package.v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MinimizeReport {
    pub schema: String,
    pub original_trace_path: PathBuf,
    pub minimized_trace_path: PathBuf,
    pub replay_report_path: PathBuf,
    pub failure_package_path: PathBuf,
    pub target_oracle_id: String,
    pub target_oracle_reason: String,
    pub original_event_count: usize,
    pub minimized_event_count: usize,
    pub removed_event_count: usize,
    pub operation_family_reductions: Vec<OperationFamilyReduction>,
    pub final_summary: AbstractWorldSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationFamilyReduction {
    pub boundary_kind: String,
    pub original_family_event_count: usize,
    pub accepted: bool,
    pub event_count_after_attempt: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FailurePackageManifest {
    schema: String,
    original_trace_path: PathBuf,
    minimized_trace: &'static str,
    replay_report: &'static str,
    oracle: &'static str,
    final_summary: &'static str,
    target_oracle: OracleVerdict,
    target_oracle_reason: String,
    original_event_count: usize,
    minimized_event_count: usize,
    operation_family_reductions: Vec<OperationFamilyReduction>,
    replay_command: String,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum MinimizeError {
    TraceIo(TraceIoError),
    Replay(ReplayError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Fixture(String),
    Target(String),
}

impl fmt::Display for MinimizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceIo(err) => write!(f, "{err}"),
            Self::Replay(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "minimize I/O failed: {err}"),
            Self::Json(err) => write!(f, "minimize JSON failed: {err}"),
            Self::Fixture(message) => {
                write!(f, "failing fixture materialization failed: {message}")
            }
            Self::Target(message) => write!(f, "minimizer target error: {message}"),
        }
    }
}

impl std::error::Error for MinimizeError {}

impl From<TraceIoError> for MinimizeError {
    fn from(value: TraceIoError) -> Self {
        Self::TraceIo(value)
    }
}

impl From<ReplayError> for MinimizeError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

impl From<std::io::Error> for MinimizeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for MinimizeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub async fn minimize_trace_or_fixture_file(
    input_path: &Path,
    artifact_root: &Path,
) -> Result<MinimizeReport, MinimizeError> {
    match read_trace(input_path) {
        Ok(trace) => minimize_trace(input_path, &trace, artifact_root),
        Err(trace_error) => {
            let fixture = read_failing_trace_fixture(input_path).map_err(|fixture_error| {
                MinimizeError::Fixture(format!(
                    "input was neither a SimulationTrace ({trace_error}) nor a failing fixture ({fixture_error})"
                ))
            })?;
            let trace = materialize_failing_fixture_trace(&fixture).await?;
            minimize_trace(input_path, &trace, artifact_root)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FailingTraceFixture {
    pub schema: String,
    pub fixture_id: String,
    pub seed: u64,
    pub profile: String,
    pub max_boundaries: usize,
    pub mutation: FailingTraceMutation,
    pub expected_oracle_id: String,
    pub expected_reason_contains: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FailingTraceMutation {
    #[serde(default)]
    pub remove_kind: Option<String>,
    #[serde(default)]
    pub remove_runtime_completion_for_kind: Option<String>,
    #[serde(default)]
    pub remove_observed_field_for_kind: Option<ObservedFieldMutation>,
    #[serde(default)]
    pub contract_execution_field: Option<ContractExecutionFieldMutation>,
    #[serde(default)]
    pub omit_process_wake_join_session: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ObservedFieldMutation {
    pub kind: String,
    pub field: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContractExecutionFieldMutation {
    pub contract: String,
    pub pointer: String,
    #[serde(default)]
    pub replacement: Option<serde_json::Value>,
}

fn read_failing_trace_fixture(path: &Path) -> Result<FailingTraceFixture, MinimizeError> {
    let body = std::fs::read_to_string(path)?;
    let fixture: FailingTraceFixture = serde_json::from_str(&body)?;
    if fixture.schema != "lash.sim.failing-trace-fixture.v1" {
        return Err(MinimizeError::Fixture(format!(
            "unsupported failing fixture schema `{}`",
            fixture.schema
        )));
    }
    Ok(fixture)
}

pub(crate) async fn materialize_failing_fixture_trace(
    fixture: &FailingTraceFixture,
) -> Result<SimulationTrace, MinimizeError> {
    let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
        .map_err(|err| MinimizeError::Fixture(err.to_string()))?;
    let mut trace = run_generated_workload_for_fixture(workload, "fixture")
        .await
        .map_err(|err| MinimizeError::Fixture(err.to_string()))?;
    apply_fixture_mutation(&mut trace, &fixture.mutation)?;
    select_fixture_target_oracle(&mut trace, fixture)?;
    if trace.oracle.status != OracleStatus::Failed {
        return Err(MinimizeError::Fixture(format!(
            "fixture `{}` produced {:?}, expected failed oracle `{}`",
            fixture.fixture_id, trace.oracle.status, fixture.expected_oracle_id
        )));
    }
    if trace.oracle.oracle_id != fixture.expected_oracle_id {
        return Err(MinimizeError::Fixture(format!(
            "fixture `{}` produced oracle `{}`, expected `{}`",
            fixture.fixture_id, trace.oracle.oracle_id, fixture.expected_oracle_id
        )));
    }
    if !trace
        .oracle
        .message
        .contains(&fixture.expected_reason_contains)
    {
        return Err(MinimizeError::Fixture(format!(
            "fixture `{}` reason `{}` did not contain `{}`",
            fixture.fixture_id, trace.oracle.message, fixture.expected_reason_contains
        )));
    }
    Ok(trace)
}

pub fn minimize_trace(
    trace_path: &Path,
    trace: &SimulationTrace,
    artifact_root: &Path,
) -> Result<MinimizeReport, MinimizeError> {
    let target_oracle_id = trace.oracle.oracle_id.clone();
    let target_status = trace.oracle.status.clone();
    let target_oracle_reason = trace.oracle.message.clone();
    let target = TargetFailure {
        oracle_id: target_oracle_id.as_str(),
        status: &target_status,
        reason: target_oracle_reason.as_str(),
    };
    if target.oracle_id == LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE {
        return Err(MinimizeError::Target(format!(
            "oracle `{}` cannot be re-evaluated from a serialized trace because its live provider failure facts are not recorded; no minimized package was written",
            target.oracle_id
        )));
    }
    let mut best = trace.clone();
    let mut operation_family_reductions = Vec::new();
    for kind in operation_families(&best) {
        let original_family_event_count = best
            .events
            .iter()
            .filter(|event| event.kind == kind)
            .count();
        if original_family_event_count == 0 {
            continue;
        }
        let mut candidate = best.clone();
        candidate.events.retain(|event| event.kind != kind);
        renumber_events(&mut candidate);
        let target_preserved =
            refresh_trace_verdicts(&mut candidate, Some(target)).unwrap_or(false);
        let accepted = target_preserved
            && preserves_target_failure(&candidate, target)
            && replay_trace(Path::new("candidate-family.trace.json"), &candidate).is_ok();
        if accepted {
            best = candidate;
        }
        operation_family_reductions.push(OperationFamilyReduction {
            boundary_kind: kind.name().to_string(),
            original_family_event_count,
            accepted,
            event_count_after_attempt: best.events.len(),
        });
    }
    let mut index = 0;
    while index < best.events.len() {
        let mut candidate = best.clone();
        candidate.events.remove(index);
        renumber_events(&mut candidate);
        let target_preserved =
            refresh_trace_verdicts(&mut candidate, Some(target)).unwrap_or(false);
        if target_preserved
            && preserves_target_failure(&candidate, target)
            && replay_trace(Path::new("candidate.trace.json"), &candidate).is_ok()
        {
            best = candidate;
        } else {
            index += 1;
        }
    }
    if !refresh_trace_verdicts(&mut best, Some(target))? {
        return Err(MinimizeError::Target(format!(
            "final candidate did not preserve target `{}` with status {:?} and reason `{}`; no minimized package was written",
            target.oracle_id, target.status, target.reason
        )));
    }

    let package_dir = artifact_root.join("minimized-regression");
    let minimized_trace_path = package_dir.join("trace.json");
    let replay_report_path = package_dir.join("replay.json");
    let package_path = package_dir.join("package.json");

    let replay = replay_trace(&minimized_trace_path, &best)?;
    std::fs::create_dir_all(artifact_root)?;
    let staging_dir = tempfile::Builder::new()
        .prefix(".minimized-regression-")
        .tempdir_in(artifact_root)?;
    let staged_minimized_trace_path = staging_dir.path().join("trace.json");
    let staged_replay_report_path = staging_dir.path().join("replay.json");
    let staged_oracle_path = staging_dir.path().join("oracle.json");
    let staged_final_summary_path = staging_dir.path().join("final-summary.json");
    let staged_package_path = staging_dir.path().join("package.json");

    write_trace(&staged_minimized_trace_path, &best)?;
    write_replay_report(&staged_replay_report_path, &replay)?;
    std::fs::write(
        &staged_oracle_path,
        serde_json::to_vec_pretty(&best.oracle)?,
    )?;
    std::fs::write(
        &staged_final_summary_path,
        serde_json::to_vec_pretty(&best.final_summary)?,
    )?;
    let package = FailurePackageManifest {
        schema: FAILURE_PACKAGE_SCHEMA.to_string(),
        original_trace_path: trace_path.to_path_buf(),
        minimized_trace: "trace.json",
        replay_report: "replay.json",
        oracle: "oracle.json",
        final_summary: "final-summary.json",
        target_oracle: best.oracle.clone(),
        target_oracle_reason: target_oracle_reason.clone(),
        original_event_count: trace.events.len(),
        minimized_event_count: best.events.len(),
        operation_family_reductions: operation_family_reductions.clone(),
        replay_command: format!(
            "cargo run -p lash-sim --locked -- replay {}",
            minimized_trace_path.display()
        ),
    };
    std::fs::write(&staged_package_path, serde_json::to_vec_pretty(&package)?)?;
    match std::fs::symlink_metadata(&package_dir) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to replace existing minimized package `{}`",
                    package_dir.display()
                ),
            )
            .into());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    std::fs::rename(staging_dir.path(), &package_dir)?;

    Ok(MinimizeReport {
        schema: MINIMIZE_REPORT_SCHEMA.to_string(),
        original_trace_path: trace_path.to_path_buf(),
        minimized_trace_path,
        replay_report_path,
        failure_package_path: package_path,
        target_oracle_id,
        target_oracle_reason,
        original_event_count: trace.events.len(),
        minimized_event_count: best.events.len(),
        removed_event_count: trace.events.len().saturating_sub(best.events.len()),
        operation_family_reductions,
        final_summary: best.final_summary,
    })
}

fn preserves_target_failure(candidate: &SimulationTrace, target: TargetFailure<'_>) -> bool {
    candidate.oracle.oracle_id == target.oracle_id
        && &candidate.oracle.status == target.status
        && candidate.oracle.message == target.reason
}

#[derive(Clone, Copy)]
struct TargetFailure<'a> {
    oracle_id: &'a str,
    status: &'a crate::trace::OracleStatus,
    reason: &'a str,
}

fn operation_families(trace: &SimulationTrace) -> Vec<BoundaryKind> {
    trace
        .events
        .iter()
        .map(|event| event.kind)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn renumber_events(trace: &mut SimulationTrace) {
    for (sequence, event) in trace.events.iter_mut().enumerate() {
        event.sequence = sequence;
    }
}

fn refresh_trace_verdicts(
    trace: &mut SimulationTrace,
    target: Option<TargetFailure<'_>>,
) -> Result<bool, MinimizeError> {
    let carried_live_provider_oracle = trace
        .oracles
        .iter()
        .find(|oracle| oracle.oracle_id == LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE)
        .cloned();
    retain_causally_supported_checkpoint_writes(trace);
    let final_summary = summary_for_trace(trace)?;
    let mut oracles = Vec::new();
    if let Some(verdict) = carried_live_provider_oracle {
        oracles.push(verdict);
    }
    oracles.extend(generated_trace_oracles(
        &trace.events,
        &final_summary,
        &trace.durable_writes,
        &trace.expectations,
    ));
    let mut oracle = combine_oracles(&oracles);
    let target_preserved = target.is_none_or(|target| {
        if verdict_matches_target(&oracle, target) {
            true
        } else if let Some(target_oracle) = find_target_oracle(&oracles, target) {
            oracle = target_oracle.clone();
            true
        } else {
            false
        }
    });
    trace.final_summary = final_summary;
    trace.oracles = oracles;
    trace.oracle = oracle;
    Ok(target_preserved)
}

fn find_target_oracle<'a>(
    oracles: &'a [OracleVerdict],
    target: TargetFailure<'_>,
) -> Option<&'a OracleVerdict> {
    oracles
        .iter()
        .find(|oracle| verdict_matches_target(oracle, target))
}

fn verdict_matches_target(oracle: &OracleVerdict, target: TargetFailure<'_>) -> bool {
    oracle.oracle_id == target.oracle_id
        && &oracle.status == target.status
        && oracle.message == target.reason
}

fn select_fixture_target_oracle(
    trace: &mut SimulationTrace,
    fixture: &FailingTraceFixture,
) -> Result<(), MinimizeError> {
    let Some(target) = trace.oracles.iter().find(|oracle| {
        oracle.status == OracleStatus::Failed
            && oracle.oracle_id == fixture.expected_oracle_id
            && oracle.message.contains(&fixture.expected_reason_contains)
    }) else {
        return Err(MinimizeError::Fixture(format!(
            "fixture `{}` did not produce expected oracle `{}` containing `{}`; primary oracle was `{}`: {}",
            fixture.fixture_id,
            fixture.expected_oracle_id,
            fixture.expected_reason_contains,
            trace.oracle.oracle_id,
            trace.oracle.message
        )));
    };
    trace.oracle = target.clone();
    Ok(())
}

fn summary_for_trace(trace: &SimulationTrace) -> Result<AbstractWorldSummary, MinimizeError> {
    let mut store = ModelStore::default();
    for event in &trace.events {
        store.apply_observed_boundary(&event.as_event(), &event.observed);
    }
    // Minimization carries only checkpoint evidence whose causal boundary
    // survived the candidate reduction. The abstract model cannot re-execute a
    // commit, so the retained records are intentionally carried as data.
    store
        .summarize_with_trace_checkpoint_writes(&trace.events, &trace.durable_writes)
        .map_err(MinimizeError::Fixture)
}

fn retain_causally_supported_checkpoint_writes(trace: &mut SimulationTrace) {
    let admitted_sessions = trace
        .events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Ingress)
        .map(|event| event.actor_alias.as_str())
        .collect::<BTreeSet<_>>();
    let retained_boundary_ids = trace
        .events
        .iter()
        .map(|event| event.boundary_id.as_str())
        .collect::<BTreeSet<_>>();
    let retained_runtime_turns = trace
        .events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Provider
                || event
                    .payload
                    .get("suspend_resume")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        })
        .map(|event| {
            let turn_index = event
                .observed
                .get("turn_index")
                .or_else(|| event.payload.get("turn_index"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1) as usize;
            (event.actor_alias.as_str(), turn_index)
        })
        .collect::<BTreeSet<_>>();
    trace.durable_writes.retain(|write| {
        admitted_sessions.contains(write.attributed_session())
            && write.cause_boundary_id.as_deref().map_or_else(
                || retained_runtime_turns.contains(&(write.attributed_session(), write.turn_index)),
                |boundary_id| retained_boundary_ids.contains(boundary_id),
            )
    });
}

fn apply_fixture_mutation(
    trace: &mut SimulationTrace,
    mutation: &FailingTraceMutation,
) -> Result<(), MinimizeError> {
    if let Some(remove_kind) = mutation.remove_kind.as_deref() {
        let kind = fixture_boundary_kind(remove_kind)?;
        let mut removed_queued_inputs = BTreeSet::new();
        if kind == BoundaryKind::QueuedIngress {
            removed_queued_inputs.extend(
                trace
                    .events
                    .iter()
                    .filter(|event| event.kind == BoundaryKind::QueuedIngress)
                    .map(|event| event.boundary_id.clone()),
            );
        }
        trace.events.retain(|event| event.kind != kind);
        if !removed_queued_inputs.is_empty() {
            rewrite_cancellations_for_removed_queued_inputs(trace, &removed_queued_inputs)?;
        }
    }
    if let Some(kind_name) = mutation.remove_runtime_completion_for_kind.as_deref() {
        let kind = fixture_boundary_kind(kind_name)?;
        let Some(event) = trace.events.iter_mut().find(|event| event.kind == kind) else {
            return Err(MinimizeError::Fixture(format!(
                "fixture target boundary kind `{kind_name}` was not present"
            )));
        };
        event
            .payload
            .as_object_mut()
            .ok_or_else(|| {
                MinimizeError::Fixture(format!(
                    "fixture target boundary kind `{kind_name}` had non-object payload"
                ))
            })?
            .remove("runtime_completion");
    }
    if let Some(remove) = mutation.remove_observed_field_for_kind.as_ref() {
        let kind = fixture_boundary_kind(&remove.kind)?;
        let mut removed = 0usize;
        for event in trace.events.iter_mut().filter(|event| event.kind == kind) {
            if event
                .observed
                .as_object_mut()
                .ok_or_else(|| {
                    MinimizeError::Fixture(format!(
                        "fixture target boundary kind `{}` had non-object observed payload",
                        remove.kind
                    ))
                })?
                .remove(&remove.field)
                .is_some()
            {
                removed += 1;
            }
        }
        if removed == 0 {
            return Err(MinimizeError::Fixture(format!(
                "fixture removed no `{}` observed fields from {kind:?}",
                remove.field
            )));
        }
    }
    if let Some(mutation) = mutation.contract_execution_field.as_ref() {
        apply_contract_execution_field_mutation(trace, mutation)?;
    }
    if mutation.omit_process_wake_join_session {
        let mut removed = 0usize;
        for event in trace
            .events
            .iter_mut()
            .filter(|event| event.kind == BoundaryKind::ProcessWake)
        {
            event
                .payload
                .as_object_mut()
                .ok_or_else(|| {
                    MinimizeError::Fixture(
                        "process wake fixture target had non-object payload".to_string(),
                    )
                })?
                .insert(
                    "omit_join_session".to_string(),
                    serde_json::Value::Bool(true),
                );
            if event
                .observed
                .as_object_mut()
                .ok_or_else(|| {
                    MinimizeError::Fixture(
                        "process wake fixture target had non-object observed payload".to_string(),
                    )
                })?
                .remove("session")
                .is_some()
            {
                removed += 1;
            }
        }
        if removed == 0 {
            return Err(MinimizeError::Fixture(
                "fixture removed no process wake join sessions".to_string(),
            ));
        }
    }
    renumber_events(trace);
    refresh_trace_verdicts(trace, None)?;
    Ok(())
}

fn apply_contract_execution_field_mutation(
    trace: &mut SimulationTrace,
    mutation: &ContractExecutionFieldMutation,
) -> Result<(), MinimizeError> {
    let mut changed = 0usize;
    for event in trace
        .events
        .iter_mut()
        .filter(|event| event.kind == BoundaryKind::Trigger)
    {
        let observed_matches = event
            .observed
            .pointer("/contract_execution/contract")
            .and_then(serde_json::Value::as_str)
            == Some(mutation.contract.as_str());
        let payload_matches = event
            .payload
            .pointer("/contract_execution/contract")
            .and_then(serde_json::Value::as_str)
            == Some(mutation.contract.as_str());
        if !observed_matches && !payload_matches {
            continue;
        }
        if observed_matches {
            changed += usize::from(mutate_contract_execution_value(
                event
                    .observed
                    .get_mut("contract_execution")
                    .ok_or_else(|| {
                        MinimizeError::Fixture(format!(
                            "contract execution `{}` had no observed payload",
                            mutation.contract
                        ))
                    })?,
                mutation,
            )?);
        }
        if payload_matches {
            changed += usize::from(mutate_contract_execution_value(
                event.payload.get_mut("contract_execution").ok_or_else(|| {
                    MinimizeError::Fixture(format!(
                        "contract execution `{}` had no scheduler payload",
                        mutation.contract
                    ))
                })?,
                mutation,
            )?);
        }
    }
    if changed == 0 {
        return Err(MinimizeError::Fixture(format!(
            "fixture did not mutate contract execution `{}` at `{}`",
            mutation.contract, mutation.pointer
        )));
    }
    Ok(())
}

fn mutate_contract_execution_value(
    execution: &mut serde_json::Value,
    mutation: &ContractExecutionFieldMutation,
) -> Result<bool, MinimizeError> {
    if let Some(replacement) = mutation.replacement.as_ref() {
        let Some(target) = execution.pointer_mut(&mutation.pointer) else {
            return Ok(false);
        };
        *target = replacement.clone();
        return Ok(true);
    }
    remove_json_pointer(execution, &mutation.pointer)
}

fn remove_json_pointer(
    value: &mut serde_json::Value,
    pointer: &str,
) -> Result<bool, MinimizeError> {
    let Some((parent_pointer, key)) = pointer.rsplit_once('/') else {
        return Err(MinimizeError::Fixture(format!(
            "contract execution mutation pointer `{pointer}` is not a nested JSON pointer"
        )));
    };
    let parent_pointer = if parent_pointer.is_empty() {
        ""
    } else {
        parent_pointer
    };
    let key = decode_json_pointer_token(key);
    let Some(parent) = value.pointer_mut(parent_pointer) else {
        return Ok(false);
    };
    match parent {
        serde_json::Value::Object(map) => Ok(map.remove(&key).is_some()),
        serde_json::Value::Array(values) => {
            let Ok(index) = key.parse::<usize>() else {
                return Ok(false);
            };
            if index < values.len() {
                values.remove(index);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

fn decode_json_pointer_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

fn rewrite_cancellations_for_removed_queued_inputs(
    trace: &mut SimulationTrace,
    removed_queued_inputs: &BTreeSet<String>,
) -> Result<(), MinimizeError> {
    for event in trace
        .events
        .iter_mut()
        .filter(|event| event.kind == BoundaryKind::Cancellation)
    {
        let Some(target) = event
            .payload
            .get("target")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if !removed_queued_inputs.contains(target) {
            continue;
        }
        let observed = event.observed.as_object_mut().ok_or_else(|| {
            MinimizeError::Fixture(format!(
                "queued-input fixture target cancellation `{}` had non-object observed payload",
                event.boundary_id
            ))
        })?;
        observed.insert("cancelled".to_string(), serde_json::Value::Bool(false));
        observed.insert(
            "cancel_outcome".to_string(),
            serde_json::Value::String("not_found".to_string()),
        );
        observed.insert(
            "target".to_string(),
            serde_json::Value::String(target.to_string()),
        );
    }
    Ok(())
}

fn fixture_boundary_kind(kind: &str) -> Result<BoundaryKind, MinimizeError> {
    match kind {
        "provider" => Ok(BoundaryKind::Provider),
        "provider_event" => Ok(BoundaryKind::ProviderEvent),
        "queued_ingress" => Ok(BoundaryKind::QueuedIngress),
        "provider_mutation" => Ok(BoundaryKind::ProviderMutation),
        "cancellation" => Ok(BoundaryKind::Cancellation),
        "exec_code" => Ok(BoundaryKind::ExecCode),
        "backend_failure" => Ok(BoundaryKind::BackendFailure),
        "process_wake" => Ok(BoundaryKind::ProcessWake),
        "process_lifecycle" => Ok(BoundaryKind::ProcessLifecycle),
        "trigger" => Ok(BoundaryKind::Trigger),
        "worker" => Ok(BoundaryKind::Worker),
        other => Err(MinimizeError::Fixture(format!(
            "unsupported fixture boundary kind `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::oracles::{
        runtime_graph_acyclic, runtime_usage_monotonic, scheduler_controlled_delivery,
    };
    use crate::runner::run_generated_workload_for_fixture;

    fn synthetic_trace(target: OracleVerdict) -> SimulationTrace {
        let summary = ModelStore::default()
            .summarize_with_trace_checkpoint_writes(&[], &[])
            .expect("empty model summary");
        SimulationTrace::new(
            1,
            "test-generator",
            "test-profile",
            "1/1",
            "test-workload",
            "0".repeat(64),
            "bundle",
            Default::default(),
            Default::default(),
            Vec::new(),
            Vec::new(),
            target.clone(),
            vec![target],
            summary,
        )
    }

    fn directory_snapshot(path: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut entries = std::fs::read_dir(path)
            .expect("read package directory")
            .map(|entry| {
                let entry = entry.expect("package entry");
                let path = entry.path();
                let contents = path
                    .is_file()
                    .then(|| std::fs::read(&path).expect("package file"));
                (PathBuf::from(entry.file_name()), contents)
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    #[tokio::test]
    async fn minimizer_writes_replayable_regression_package() {
        let workload = generate_workload(11, "fast-random", 24).expect("workload");
        let trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let tmp = tempfile::tempdir().expect("tempdir");

        let report = minimize_trace(Path::new("trace.json"), &trace, tmp.path()).expect("minimize");

        assert_eq!(report.schema, MINIMIZE_REPORT_SCHEMA);
        assert!(report.minimized_trace_path.exists());
        assert!(report.replay_report_path.exists());
        assert!(report.failure_package_path.exists());
        assert!(report.minimized_event_count <= report.original_event_count);
    }

    #[test]
    fn existing_minimized_package_is_never_partially_replaced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = synthetic_trace(scheduler_controlled_delivery(&[]));
        minimize_trace(Path::new("first.trace.json"), &first, tmp.path())
            .expect("first publication");
        let package_dir = tmp.path().join("minimized-regression");
        let replay_path = package_dir.join("replay.json");
        std::fs::remove_file(&replay_path).expect("remove first replay report");
        std::fs::create_dir(&replay_path).expect("obstruct replay report");
        let before = directory_snapshot(&package_dir);
        let second = synthetic_trace(runtime_usage_monotonic(&[]));

        let err = minimize_trace(Path::new("second.trace.json"), &second, tmp.path())
            .expect_err("an existing package must be refused");

        assert!(
            matches!(
                &err,
                MinimizeError::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists
            ),
            "{err}"
        );
        assert_eq!(directory_snapshot(&package_dir), before);
        assert_eq!(
            std::fs::read_dir(tmp.path())
                .expect("artifact root")
                .count(),
            1,
            "failed publication must clean its staging directory"
        );
    }

    #[test]
    fn failed_fresh_package_publication_leaves_no_completed_package() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let package_dir = tmp.path().join("minimized-regression");
        std::fs::write(&package_dir, b"publication obstruction").expect("obstruct final name");
        let trace = synthetic_trace(scheduler_controlled_delivery(&[]));

        let err = minimize_trace(Path::new("trace.json"), &trace, tmp.path())
            .expect_err("an obstructed fresh publication must fail");

        assert!(matches!(err, MinimizeError::Io(_)), "{err}");
        assert_eq!(
            std::fs::read(&package_dir).expect("unchanged obstruction"),
            b"publication obstruction"
        );
        assert_eq!(
            std::fs::read_dir(tmp.path())
                .expect("artifact root")
                .count(),
            1,
            "failed publication must clean its staging directory"
        );
        assert!(!package_dir.join("package.json").exists());
    }

    #[tokio::test]
    async fn minimizer_preserves_runtime_graph_failure_across_every_artifact() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let rows = trace
            .durable_writes
            .iter_mut()
            .filter_map(|write| write.state.as_mut())
            .filter_map(|state| state.accepted_raw_rows.as_mut())
            .filter_map(|raw| raw.get_mut("graph_nodes"))
            .filter_map(serde_json::Value::as_array_mut)
            .find(|rows| !rows.is_empty())
            .expect("seed 5 records accepted raw graph rows");
        rows.push(rows[0].clone());
        let target = runtime_graph_acyclic(&trace.durable_writes);
        assert_eq!(target.status, OracleStatus::Failed);
        assert!(target.message.contains("duplicate row"));
        trace.oracle = target.clone();
        let tmp = tempfile::tempdir().expect("tempdir");

        let report = minimize_trace(Path::new("failing.trace.json"), &trace, tmp.path())
            .expect("minimize runtime graph failure");
        let minimized = read_trace(&report.minimized_trace_path).expect("minimized trace");
        let oracle: OracleVerdict = serde_json::from_slice(
            &std::fs::read(
                report
                    .failure_package_path
                    .parent()
                    .expect("package directory")
                    .join("oracle.json"),
            )
            .expect("oracle artifact"),
        )
        .expect("oracle artifact JSON");
        let package: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&report.failure_package_path).expect("failure package"),
        )
        .expect("failure package JSON");

        assert_eq!(report.target_oracle_id, target.oracle_id);
        assert_eq!(report.target_oracle_reason, target.message);
        assert_eq!(minimized.oracle.oracle_id, report.target_oracle_id);
        assert_eq!(minimized.oracle.status, OracleStatus::Failed);
        assert_eq!(minimized.oracle.message, report.target_oracle_reason);
        assert_eq!(oracle.oracle_id, report.target_oracle_id);
        assert_eq!(oracle.status, OracleStatus::Failed);
        assert_eq!(oracle.message, report.target_oracle_reason);
        assert_eq!(
            package
                .pointer("/target_oracle/oracle_id")
                .and_then(serde_json::Value::as_str),
            Some(report.target_oracle_id.as_str())
        );
        assert_eq!(
            package
                .pointer("/target_oracle/status")
                .and_then(serde_json::Value::as_str),
            Some("failed")
        );
        assert_eq!(
            package
                .pointer("/target_oracle/message")
                .and_then(serde_json::Value::as_str),
            Some(report.target_oracle_reason.as_str())
        );
        assert_eq!(
            package
                .get("target_oracle_reason")
                .and_then(serde_json::Value::as_str),
            Some(report.target_oracle_reason.as_str())
        );
    }

    #[tokio::test]
    async fn replay_failure_publishes_no_minimized_package_artifacts() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let usage = trace
            .events
            .iter_mut()
            .filter(|event| event.kind == BoundaryKind::Provider)
            .find_map(|event| {
                event
                    .observed
                    .pointer_mut("/runtime_invariant_facts/usage/usage_events_monotonic")
            })
            .expect("provider event records usage invariant facts");
        *usage = serde_json::Value::Bool(false);
        let target = runtime_usage_monotonic(&trace.events);
        assert_eq!(target.status, OracleStatus::Failed);
        trace.oracle = target;
        let tmp = tempfile::tempdir().expect("tempdir");

        let err = minimize_trace(Path::new("failing.trace.json"), &trace, tmp.path())
            .expect_err("replay must reject contradictory runtime facts");

        assert!(matches!(err, MinimizeError::Replay(_)), "{err}");
        assert!(
            !tmp.path().join("minimized-regression").exists(),
            "a failed final replay must not publish a partial package"
        );
    }

    #[test]
    fn unsupported_live_provider_target_is_diagnosed_without_artifacts() {
        let target = OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
            "recorded live provider failure",
        );
        let trace = SimulationTrace::new(
            1,
            "test-generator",
            "test-profile",
            "1/1",
            "test-workload",
            "0".repeat(64),
            "bundle",
            Default::default(),
            Default::default(),
            Vec::new(),
            Vec::new(),
            target.clone(),
            vec![target],
            AbstractWorldSummary::with_digest(0, 0, Vec::new(), Vec::new(), Vec::new()),
        );
        let tmp = tempfile::tempdir().expect("tempdir");

        let err = minimize_trace(Path::new("live-failure.trace.json"), &trace, tmp.path())
            .expect_err("live-only target must be unsupported");

        assert!(matches!(err, MinimizeError::Target(_)), "{err}");
        assert!(err.to_string().contains("cannot be re-evaluated"));
        assert!(!tmp.path().join("minimized-regression").exists());
    }

    #[tokio::test]
    async fn removing_a_sessions_provider_family_removes_its_checkpoint_evidence() {
        let workload = generate_workload(1, "fast-random", 72).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "checkpoint-causality")
            .await
            .expect("trace");
        let contract_attributions = trace
            .durable_writes
            .iter()
            .filter(|write| write.cause_boundary_id.is_some())
            .map(|write| write.attributed_session().to_string())
            .collect::<BTreeSet<_>>();
        let target = trace
            .durable_writes
            .iter()
            .find(|write| {
                write.cause_boundary_id.is_none()
                    && !contract_attributions.contains(write.attributed_session())
            })
            .map(|write| write.attributed_session().to_string())
            .expect("generated runtime session outside contract attribution");
        assert!(
            trace
                .durable_writes
                .iter()
                .any(|write| write.attributed_session() == target),
            "fixture must start with checkpoint writes for {target}"
        );

        trace
            .events
            .retain(|event| event.actor_alias != target || event.kind != BoundaryKind::Provider);
        renumber_events(&mut trace);
        refresh_trace_verdicts(&mut trace, None).expect("refresh minimized trace");

        assert!(
            trace
                .durable_writes
                .iter()
                .all(|write| write.attributed_session() != target),
            "removed provider family retained phantom checkpoint writes: {:#?}",
            trace.durable_writes
        );
        let summary = trace
            .final_summary
            .sessions
            .iter()
            .find(|session| session.alias == target)
            .expect("target session summary");
        assert_eq!(summary.checkpoint_commit_count, 0);
        let transcript = trace.render_transcript();
        assert!(
            !transcript
                .lines()
                .any(|line| { line.starts_with(&target) && line.contains("Checkpoint") }),
            "minimized transcript retained phantom checkpoint evidence:\n{transcript}"
        );
    }

    #[tokio::test]
    async fn minimizer_preserves_failing_oracle_id_and_reason() {
        let fixture: FailingTraceFixture = serde_json::from_str(include_str!(
            "../failure-fixtures/operational-coverage-missing-cancellation.json"
        ))
        .expect("fixture");
        let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
            .expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
        select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
        assert_minimized_fixture_preserves_failure(&fixture, trace);
    }

    #[tokio::test]
    async fn minimizer_preserves_scheduler_owned_boundary_bug_reason() {
        let fixture: FailingTraceFixture = serde_json::from_str(include_str!(
            "../failure-fixtures/scheduler-owned-provider-completion-missing-evidence.json"
        ))
        .expect("fixture");
        let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
            .expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
        select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
        assert_minimized_fixture_preserves_failure(&fixture, trace);
    }

    #[tokio::test]
    async fn minimizer_preserves_rlm_mini_oracle_fixture_reason() {
        let fixture: FailingTraceFixture = serde_json::from_str(include_str!(
            "../failure-fixtures/rlm-lashlang-cell-missing-continuation.json"
        ))
        .expect("fixture");
        let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
            .expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
        select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
        let report = assert_minimized_fixture_preserves_failure(&fixture, trace);
        assert!(
            report.removed_event_count > 0,
            "RLM mini-oracle fixture should allow dependency-aware event reduction"
        );
    }

    #[tokio::test]
    async fn minimizer_preserves_agent_mini_oracle_fixture_reason() {
        let fixture: FailingTraceFixture = serde_json::from_str(include_str!(
            "../failure-fixtures/agent-parallel-join-missing-wake-session.json"
        ))
        .expect("fixture");
        let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
            .expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
        select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
        let report = assert_minimized_fixture_preserves_failure(&fixture, trace);
        assert!(
            report.removed_event_count > 0,
            "Agent mini-oracle fixture should allow dependency-aware event reduction"
        );
    }

    #[tokio::test]
    async fn minimizer_preserves_standard_mini_oracle_fixture_reason() {
        let fixture: FailingTraceFixture = serde_json::from_str(include_str!(
            "../failure-fixtures/standard-provider-error-missing-parser-matrix.json"
        ))
        .expect("fixture");
        let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
            .expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
        select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
        let report = assert_minimized_fixture_preserves_failure(&fixture, trace);
        assert!(
            report.removed_event_count > 0,
            "Standard mini-oracle fixture should allow dependency-aware event reduction"
        );
    }

    async fn assert_named_contract_fixture(fixture_body: &str) {
        let fixture: FailingTraceFixture = serde_json::from_str(fixture_body).expect("fixture");
        let workload = generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
            .expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
        select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
        assert_minimized_fixture_preserves_failure(&fixture, trace);
    }

    #[tokio::test]
    async fn minimizer_preserves_standard_max_turn_stop_fixture_reason() {
        assert_named_contract_fixture(include_str!(
            "../failure-fixtures/standard-max-turn-stop-missing.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn minimizer_preserves_rlm_typed_finish_fixture_reason() {
        assert_named_contract_fixture(include_str!(
            "../failure-fixtures/rlm-typed-finish-terminal-event-missing.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn minimizer_preserves_rlm_default_mode_fixture_reason() {
        assert_named_contract_fixture(include_str!(
            "../failure-fixtures/rlm-empty-options-default-mode-broken.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn minimizer_preserves_agent_tuple_fixture_reason() {
        assert_named_contract_fixture(include_str!(
            "../failure-fixtures/agent-tuple-json-array-shape-broken.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn minimizer_preserves_agent_subagent_child_fixture_reason() {
        assert_named_contract_fixture(include_str!(
            "../failure-fixtures/agent-started-process-subagent-child-graph-missing.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn minimizer_preserves_agent_failed_child_fixture_reason() {
        assert_named_contract_fixture(include_str!(
            "../failure-fixtures/agent-failed-child-task-fail-evidence-missing.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn minimizer_preserves_provider_worker_backend_fixture_reasons() {
        for fixture_body in [
            include_str!("../failure-fixtures/provider-mutation-runtime-completion-missing.json"),
            include_str!("../failure-fixtures/worker-failover-stale-rejection-missing.json"),
            include_str!("../failure-fixtures/backend-retry-runtime-completion-missing.json"),
            include_str!("../failure-fixtures/queued-input-operational-missing.json"),
            include_str!("../failure-fixtures/trigger-wakeup-operational-missing.json"),
            include_str!("../failure-fixtures/process-wake-operational-missing.json"),
        ] {
            let fixture: FailingTraceFixture = serde_json::from_str(fixture_body).expect("fixture");
            let workload =
                generate_workload(fixture.seed, &fixture.profile, fixture.max_boundaries)
                    .expect("workload");
            let mut trace = run_generated_workload_for_fixture(workload, "bundle")
                .await
                .expect("trace");
            apply_fixture_mutation(&mut trace, &fixture.mutation).expect("mutation");
            select_fixture_target_oracle(&mut trace, &fixture).expect("target oracle");
            assert_minimized_fixture_preserves_failure(&fixture, trace);
        }
    }

    fn assert_minimized_fixture_preserves_failure(
        fixture: &FailingTraceFixture,
        trace: SimulationTrace,
    ) -> MinimizeReport {
        assert_eq!(trace.oracle.status, OracleStatus::Failed);
        assert_eq!(trace.oracle.oracle_id, fixture.expected_oracle_id);
        let expected_oracle_id = trace.oracle.oracle_id.clone();
        let expected_reason = trace.oracle.message.clone();
        assert!(expected_reason.contains(&fixture.expected_reason_contains));
        let tmp = tempfile::tempdir().expect("tempdir");

        let report =
            minimize_trace(Path::new("failing.trace.json"), &trace, tmp.path()).expect("minimize");
        let minimized = read_trace(&report.minimized_trace_path).expect("minimized trace");

        assert_eq!(report.target_oracle_id, expected_oracle_id);
        assert_eq!(report.target_oracle_reason, expected_reason);
        assert_eq!(minimized.oracle.oracle_id, expected_oracle_id);
        assert_eq!(minimized.oracle.status, OracleStatus::Failed);
        assert_eq!(minimized.oracle.message, expected_reason);
        assert!(report.minimized_event_count <= report.original_event_count);
        report
    }
}
