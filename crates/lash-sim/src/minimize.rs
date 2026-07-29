use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::generator::generate_workload;
use crate::oracles::{
    abandoned_requires_evidence, backend_failure_observed, cancellation_observed, combine_oracles,
    cross_session_isolation, durable_effect_exactly_once, exec_code_observed,
    generated_final_value_semantic_channel, generated_suspend_resume, healthy_long_turn_liveness,
    ingress_sessions_opened, lease_time_monotonic, observer_convergence,
    observer_reconnect_observed, operational_coverage, process_never_double_started,
    process_wake_observed, provider_mutation_rejected, provider_transport_mutation_classified,
    provider_turn_interleaving_depth, queued_ingress_observed, runtime_session_graph_contract,
    scenario_contract_mini_oracles, scenario_contract_oracles, scheduler_controlled_delivery,
    scheduler_owned_runtime_completions, state_machine_semantic_invariants, tool_boundary_observed,
    trigger_delivery_observed, worker_failover_continues_work, worker_stale_completion_rejected,
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
pub enum MinimizeError {
    TraceIo(TraceIoError),
    Replay(ReplayError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Fixture(String),
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
    std::fs::create_dir_all(artifact_root)?;
    let target_oracle_id = trace.oracle.oracle_id.clone();
    let target_status = trace.oracle.status.clone();
    let target_oracle_reason = trace.oracle.message.clone();
    let target = TargetFailure {
        oracle_id: target_oracle_id.as_str(),
        status: &target_status,
        reason: target_oracle_reason.as_str(),
    };
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
        refresh_trace_verdicts(&mut candidate, Some(target));
        let accepted = preserves_target_failure(&candidate, target)
            && replay_trace(Path::new("candidate-family.trace.json"), &candidate).is_ok();
        if accepted {
            best = candidate;
        }
        operation_family_reductions.push(OperationFamilyReduction {
            boundary_kind: boundary_kind_name(kind).to_string(),
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
        refresh_trace_verdicts(&mut candidate, Some(target));
        if preserves_target_failure(&candidate, target)
            && replay_trace(Path::new("candidate.trace.json"), &candidate).is_ok()
        {
            best = candidate;
        } else {
            index += 1;
        }
    }
    refresh_trace_verdicts(&mut best, Some(target));

    let package_dir = artifact_root.join("minimized-regression");
    std::fs::create_dir_all(&package_dir)?;
    let minimized_trace_path = package_dir.join("trace.json");
    let replay_report_path = package_dir.join("replay.json");
    let oracle_path = package_dir.join("oracle.json");
    let final_summary_path = package_dir.join("final-summary.json");
    let package_path = package_dir.join("package.json");

    write_trace(&minimized_trace_path, &best)?;
    let replay = replay_trace(&minimized_trace_path, &best)?;
    write_replay_report(&replay_report_path, &replay)?;
    std::fs::write(&oracle_path, serde_json::to_vec_pretty(&best.oracle)?)?;
    std::fs::write(
        &final_summary_path,
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
    std::fs::write(&package_path, serde_json::to_vec_pretty(&package)?)?;

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

fn boundary_kind_name(kind: BoundaryKind) -> &'static str {
    match kind {
        BoundaryKind::Ingress => "ingress",
        BoundaryKind::QueuedIngress => "queued_ingress",
        BoundaryKind::Provider => "provider",
        BoundaryKind::ProviderEvent => "provider_event",
        BoundaryKind::Tool => "tool",
        BoundaryKind::ExecCode => "exec_code",
        BoundaryKind::DurableEffect => "durable_effect",
        BoundaryKind::ProcessWake => "process_wake",
        BoundaryKind::ProcessLifecycle => "process_lifecycle",
        BoundaryKind::Worker => "worker",
        BoundaryKind::Observer => "observer",
        BoundaryKind::Cancellation => "cancellation",
        BoundaryKind::Trigger => "trigger",
        BoundaryKind::BackendFailure => "backend_failure",
        BoundaryKind::ProviderMutation => "provider_mutation",
        BoundaryKind::LeaseTime => "lease_time",
    }
}

fn renumber_events(trace: &mut SimulationTrace) {
    for (sequence, event) in trace.events.iter_mut().enumerate() {
        event.sequence = sequence;
    }
}

fn refresh_trace_verdicts(trace: &mut SimulationTrace, target: Option<TargetFailure<'_>>) {
    let final_summary = summary_for_trace(trace);
    let oracles = generated_oracles(&trace.events, &final_summary);
    let mut oracle = combine_oracles(&oracles);
    if let Some(target) = target
        && let Some(target_oracle) = find_target_oracle(&oracles, target)
    {
        oracle = target_oracle.clone();
    }
    trace.final_summary = final_summary;
    trace.oracles = oracles;
    trace.oracle = oracle;
}

fn find_target_oracle<'a>(
    oracles: &'a [OracleVerdict],
    target: TargetFailure<'_>,
) -> Option<&'a OracleVerdict> {
    oracles.iter().find(|oracle| {
        oracle.oracle_id == target.oracle_id
            && &oracle.status == target.status
            && oracle.message == target.reason
    })
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

fn summary_for_trace(trace: &SimulationTrace) -> AbstractWorldSummary {
    let mut store = ModelStore::default();
    for event in &trace.events {
        store.apply_observed_boundary(&event.as_event(), &event.observed);
    }
    store.apply_durable_writes(&trace.durable_writes);
    store.summary()
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
    refresh_trace_verdicts(trace, None);
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

fn generated_oracles(
    events: &[crate::scheduler::DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> Vec<OracleVerdict> {
    let mut oracles = vec![
        scheduler_controlled_delivery(events),
        scheduler_owned_runtime_completions(events),
        state_machine_semantic_invariants(events, summary),
        operational_coverage(events, summary),
        ingress_sessions_opened(summary),
        queued_ingress_observed(summary, events),
        cancellation_observed(summary, events),
        trigger_delivery_observed(summary, events),
        observer_reconnect_observed(summary, events),
        backend_failure_observed(summary, events),
        provider_mutation_rejected(summary, events),
        provider_transport_mutation_classified(events),
        provider_turn_interleaving_depth(events),
        process_wake_observed(summary, events),
        process_never_double_started(events),
        abandoned_requires_evidence(events),
        tool_boundary_observed(summary, events),
        exec_code_observed(summary, events),
        cross_session_isolation(summary),
        observer_convergence(summary),
        runtime_session_graph_contract(summary),
        durable_effect_exactly_once(summary),
        worker_stale_completion_rejected(summary),
        worker_failover_continues_work(events),
        healthy_long_turn_liveness(events),
        lease_time_monotonic(events),
        generated_suspend_resume(events),
        generated_final_value_semantic_channel(events),
    ];
    oracles.extend(scenario_contract_mini_oracles(events, summary));
    oracles.extend(scenario_contract_oracles(events, summary));
    oracles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

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
            "../failure-fixtures/rlm-lashlang-cell-missing-exec-outcome.json"
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

    #[tokio::test]
    async fn minimizer_preserves_named_contract_execution_fixture_reasons() {
        for fixture_body in [
            include_str!("../failure-fixtures/standard-max-turn-stop-missing.json"),
            include_str!("../failure-fixtures/rlm-typed-finish-terminal-event-missing.json"),
            include_str!("../failure-fixtures/rlm-empty-options-default-mode-broken.json"),
            include_str!("../failure-fixtures/agent-tuple-json-array-shape-broken.json"),
            include_str!(
                "../failure-fixtures/agent-started-process-subagent-child-graph-missing.json"
            ),
            include_str!("../failure-fixtures/agent-failed-child-task-fail-evidence-missing.json"),
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
