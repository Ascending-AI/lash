use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::scheduler::DeliveredBoundary;

pub const TRACE_SCHEMA: &str = "lash.sim.trace.v1";
pub const TRACE_EVENT_LINE_SCHEMA: &str = "lash.sim.trace-event-line.v1";
pub const REPLAY_REPORT_SCHEMA: &str = "lash.sim.replay-report.v1";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StableAliases {
    aliases: BTreeMap<String, String>,
    next_by_prefix: BTreeMap<String, usize>,
}

impl StableAliases {
    pub fn alias(&mut self, prefix: &str, raw_id: impl Into<String>) -> String {
        let raw_id = raw_id.into();
        if let Some(alias) = self.aliases.get(&raw_id) {
            return alias.clone();
        }
        let next = self.next_by_prefix.entry(prefix.to_string()).or_insert(0);
        *next += 1;
        let alias = format!("{prefix}-{next:03}");
        self.aliases.insert(raw_id, alias.clone());
        alias
    }

    pub fn into_map(self) -> BTreeMap<String, String> {
        self.aliases
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderTurnSummary {
    pub output: String,
    pub exchange_count: Option<u64>,
    pub graph_node_count: Option<u64>,
    pub transcript_message_count: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionAbstractSummary {
    pub alias: String,
    pub opened: bool,
    pub ingress_count: usize,
    pub provider_turns: Vec<ProviderTurnSummary>,
    pub tool_outputs: Vec<String>,
    pub exec_code_outputs: Vec<String>,
    pub observer_turn_indices: Vec<usize>,
    pub observer_reconnects: usize,
    pub queued_ingress_count: usize,
    pub cancellation_count: usize,
    pub trigger_count: usize,
    pub backend_failure_count: usize,
    pub provider_mutation_count: usize,
    pub process_wake_count: usize,
    // Defaulted and omitted when zero so traces recorded before the
    // process-lifecycle boundary (which have no recovery scenario, count 0) keep
    // their exact recorded summary digest.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub process_lifecycle_count: usize,
    pub durable_effect_keys: Vec<String>,
    pub lease_time_ticks: Vec<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub checkpoint_commit_count: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub checkpoint_component_stored_count: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub checkpoint_component_ref_count: usize,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub checkpoint_head_revision: u64,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableEffectAbstractSummary {
    pub durable_key: String,
    pub execution_count: usize,
    pub replay_count: usize,
    pub result_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerAbstractSummary {
    pub worker_alias: String,
    pub session_alias: String,
    pub active_incarnation_id: String,
    pub active_fencing_token: u64,
    pub lease_owner_changes: usize,
    pub stale_completion_rejections: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_stale_completion_rejected: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_stale_output_absent: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub process_terminal_writer: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub process_terminal_event_count: usize,
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub const WORKLOAD_EXPECTATIONS_SCHEMA: &str = "lash.sim.workload-expectations.v1";

/// Observations the *workload* declared before the run started, derived from
/// the generated plan rather than from anything the run produced.
///
/// Universally-quantified oracles are vacuously true over an empty observation
/// set, so a generator, delivery-path or projection break that yields zero
/// observations would otherwise read as compliance. Every oracle that evaluates
/// such a law compares what it saw against what is declared here and fails when
/// a declared observation class is absent. The declaration is never observed,
/// so "expected 5 sessions and saw 0" is provable rather than an ad hoc
/// `is_empty()` suspicion.
///
/// Sessions are declared by *identity*, not by count. A count only supports a
/// cardinality comparison, which is sound only when the observed population is
/// exactly the declared one; the independent checkpoint checker's population is
/// strictly wider (it also reconstructs suspend- and worker-attributed
/// commits), so `observed >= declared` there would still pass a run in which
/// every declared session lost its checkpoints. Identities make the floor say
/// which session went missing.
///
/// Traces recorded before this declaration existed deserialize to
/// [`WorkloadExpectations::default()`]: no session aliases and every count
/// zero, which declares nothing and therefore imposes no floor — those fixtures
/// are replayed, not oracle-evaluated.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkloadExpectations {
    #[serde(default)]
    pub schema: String,
    /// Stable aliases of the sessions the workload planned. Every one is
    /// expected to open, converge an observer, advance its session graph and
    /// commit checkpoints under that alias.
    #[serde(default)]
    pub sessions: Vec<String>,
    /// Provider-turn boundaries the workload planned.
    pub provider_turn_count: usize,
    /// Provider-mutation boundaries whose mutation is a transport/HTTP class.
    pub transport_mutation_count: usize,
    /// Lease-time boundaries the workload planned.
    pub lease_time_boundary_count: usize,
}

impl WorkloadExpectations {
    /// Declare the observations explicitly. Callers that derive them from a
    /// generated workload use [`crate::generator::GeneratedWorkload::expectations`].
    pub fn new(
        sessions: Vec<String>,
        provider_turn_count: usize,
        transport_mutation_count: usize,
        lease_time_boundary_count: usize,
    ) -> Self {
        Self {
            schema: WORKLOAD_EXPECTATIONS_SCHEMA.to_string(),
            sessions,
            provider_turn_count,
            transport_mutation_count,
            lease_time_boundary_count,
        }
    }

    /// How many sessions the workload declared.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Declared session aliases absent from an observed session population,
    /// in declaration order. Empty when every declared session is present —
    /// including when nothing was declared at all.
    pub fn sessions_missing_from<'a>(
        &'a self,
        observed: impl IntoIterator<Item = &'a str>,
    ) -> Vec<&'a str> {
        let observed = observed.into_iter().collect::<BTreeSet<_>>();
        self.sessions
            .iter()
            .map(String::as_str)
            .filter(|alias| !observed.contains(alias))
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AbstractWorldSummary {
    pub session_count: usize,
    pub total_events: usize,
    pub sessions: Vec<SessionAbstractSummary>,
    pub durable_effects: Vec<DurableEffectAbstractSummary>,
    pub workers: Vec<WorkerAbstractSummary>,
    pub digest: String,
}

impl AbstractWorldSummary {
    pub fn with_digest(
        session_count: usize,
        total_events: usize,
        sessions: Vec<SessionAbstractSummary>,
        durable_effects: Vec<DurableEffectAbstractSummary>,
        workers: Vec<WorkerAbstractSummary>,
    ) -> Self {
        let digest = summary_digest(
            session_count,
            total_events,
            &sessions,
            &durable_effects,
            &workers,
        );
        Self {
            session_count,
            total_events,
            sessions,
            durable_effects,
            workers,
            digest,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleStatus {
    Passed,
    Failed,
}

/// Whether a verdict is grounded in an independently observed production
/// boundary or states a property of the simulator/model itself.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleObservationClass {
    RealObservation,
    ModelProperty,
}

pub fn oracle_observation_class(oracle_id: &str) -> Option<OracleObservationClass> {
    use OracleObservationClass::{ModelProperty, RealObservation};

    match oracle_id {
        "sim.oracle.generated-workload.v1"
        | "sim.oracle.state-machine-semantic-invariants.v1"
        | "sim.oracle.operational-coverage.v1"
        | "sim.oracle.cross-session-isolation.v1"
        | "sim.oracle.observer-convergence.v1"
        | "sim.oracle.runtime-session-graph.v1"
        | "sim.oracle.replay-determinism.v1"
        | "sim.oracle.scheduler-controlled-delivery.v1"
        | "sim.oracle.scheduler-owned-runtime-completions.v1"
        | "sim.oracle.provider-turn-interleaving-depth.v1"
        | "sim.oracle.sqlite-model-replay.v1"
        | "sim.oracle.postgres-model-replay.v1" => Some(ModelProperty),
        id if id.starts_with("sim.oracle.scenario.")
            || id.starts_with("sim.oracle.scenario-mini.") =>
        {
            Some(ModelProperty)
        }
        "runtime.turn_contract"
        | "sim.oracle.abandoned-requires-evidence.v1"
        | "sim.oracle.backend-failure-observed.v1"
        | "sim.oracle.cancellation-observed.v1"
        | "sim.oracle.durable-effect-exactly-once.v1"
        | "sim.oracle.exec-code-observed.v1"
        | "sim.oracle.frame-switch-ordering.v1"
        | "sim.oracle.frame-switch-outbox-atomicity.v1"
        | "sim.oracle.frame-switch-seed.v1"
        | "sim.oracle.generated-final-value-semantic-channel.v1"
        | "sim.oracle.generated-runtime-provider-matrix.v1"
        | "sim.oracle.generated-suspend-resume.v1"
        | "sim.oracle.healthy-long-turn-liveness.v1"
        | "sim.oracle.independent-checkpoint-state.v1"
        | "sim.oracle.ingress-session-opened.v1"
        | "sim.oracle.lease-time-monotonic.v1"
        | "sim.oracle.live-provider-failure-coverage.v1"
        | "sim.oracle.live-provider-failure-terminalizes.v1"
        | "sim.oracle.logical-turn-claim-exactly-once.v1"
        | "sim.oracle.observer-reconnect.v1"
        | "sim.oracle.pending-tool-completion-through-turn.v1"
        | "sim.oracle.postgres-boundary-replay.v1"
        | "sim.oracle.postgres-checkpoint-replay.v1"
        | "sim.oracle.process-never-double-started.v1"
        | "sim.oracle.process-wake-at-most-once-runtime-turn.v1"
        | "sim.oracle.process-wake-observed.v1"
        | "sim.oracle.provider-mutation-rejected.v1"
        | "sim.oracle.provider-transport-mutation-classified.v1"
        | "sim.oracle.queued-ingress-observed.v1"
        | "sim.oracle.runtime-final-value-semantic-channel.v1"
        | "sim.oracle.runtime-graph-acyclic.v1"
        | "sim.oracle.runtime-provider-turn.v1"
        | "sim.oracle.runtime-single-active-agent-frame.v1"
        | "sim.oracle.runtime-usage-conservation.v1"
        | "sim.oracle.runtime-usage-monotonic.v1"
        | "sim.oracle.sqlite-abort-after-begin.v1"
        | "sim.oracle.sqlite-abort-before-commit.v1"
        | "sim.oracle.sqlite-boundary-replay.v1"
        | "sim.oracle.sqlite-checkpoint-replay.v1"
        | "sim.oracle.sqlite-commit-io.v1"
        | "sim.oracle.sqlite-fault-harness.v1"
        | "sim.oracle.sqlite-fault-no-duplicate-effect.v1"
        | "sim.oracle.sqlite-fault-preserves-committed-work.v1"
        | "sim.oracle.sqlite-fault-typed-error.v1"
        | "sim.oracle.sqlite-reopen-mid-sequence.v1"
        | "sim.oracle.sqlite-reopen-preserves-committed-work.v1"
        | "sim.oracle.tool-boundary-observed.v1"
        | "sim.oracle.trigger-delivery-observed.v1"
        | "sim.oracle.worker-failover-continues-work.v1"
        | "sim.oracle.worker-stale-completion-rejected.v1" => Some(RealObservation),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OracleVerdict {
    pub status: OracleStatus,
    pub oracle_id: String,
    pub observation_class: OracleObservationClass,
    pub message: String,
}

impl OracleVerdict {
    pub fn passed(oracle_id: impl Into<String>, message: impl Into<String>) -> Self {
        let oracle_id = oracle_id.into();
        let observation_class = oracle_observation_class(&oracle_id)
            .unwrap_or_else(|| panic!("oracle `{oracle_id}` has no observation-class declaration"));
        Self {
            status: OracleStatus::Passed,
            observation_class,
            oracle_id,
            message: message.into(),
        }
    }

    pub fn failed(oracle_id: impl Into<String>, message: impl Into<String>) -> Self {
        let oracle_id = oracle_id.into();
        let observation_class = oracle_observation_class(&oracle_id)
            .unwrap_or_else(|| panic!("oracle `{oracle_id}` has no observation-class declaration"));
        Self {
            status: OracleStatus::Failed,
            observation_class,
            oracle_id,
            message: message.into(),
        }
    }

    pub fn is_passed(&self) -> bool {
        self.status == OracleStatus::Passed
    }
}

#[cfg(test)]
mod observation_class_tests {
    use super::*;

    #[test]
    fn oracle_registration_marks_real_and_model_observations() {
        assert_eq!(
            oracle_observation_class("sim.oracle.backend-failure-observed.v1"),
            Some(OracleObservationClass::RealObservation)
        );
        assert_eq!(
            oracle_observation_class("sim.oracle.runtime-graph-acyclic.v1"),
            Some(OracleObservationClass::RealObservation)
        );
        assert_eq!(
            oracle_observation_class("sim.oracle.scenario.runtime-contract.v1:example"),
            Some(OracleObservationClass::ModelProperty)
        );
        assert_eq!(
            oracle_observation_class("sim.oracle.state-machine-semantic-invariants.v1"),
            Some(OracleObservationClass::ModelProperty)
        );
        assert_eq!(
            oracle_observation_class("sim.oracle.sqlite-model-replay.v1"),
            Some(OracleObservationClass::ModelProperty)
        );
        assert_eq!(
            oracle_observation_class("sim.oracle.postgres-model-replay.v1"),
            Some(OracleObservationClass::ModelProperty)
        );
        assert_eq!(
            oracle_observation_class("sim.oracle.brand-new-oracle-nobody-classified.v1"),
            None
        );
    }

    #[test]
    #[should_panic(expected = "has no observation-class declaration")]
    fn unclassified_oracle_cannot_construct_a_verdict() {
        OracleVerdict::passed(
            "sim.oracle.brand-new-oracle-nobody-classified.v1",
            "must not fail open",
        );
    }

    #[test]
    fn observation_class_is_required_when_deserializing() {
        let missing_class = serde_json::json!({
            "status": "passed",
            "oracle_id": "sim.oracle.replay-determinism.v1",
            "message": "legacy trace"
        });
        assert!(serde_json::from_value::<OracleVerdict>(missing_class).is_err());
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SimulationTrace {
    pub schema: String,
    pub seed: u64,
    pub generator_version: String,
    pub profile: String,
    pub shard: String,
    pub workload_family: String,
    pub workload_id: String,
    pub script_bundle_hash: String,
    /// Observation counts the workload declared, so oracles can distinguish
    /// "the class was absent" from "the class was never expected". Defaulted
    /// for traces recorded before the declaration existed.
    #[serde(default)]
    pub expectations: WorkloadExpectations,
    pub aliases: BTreeMap<String, String>,
    pub events: Vec<DeliveredBoundary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub durable_writes: Vec<crate::store::CheckpointWriteEvent>,
    pub oracle: OracleVerdict,
    pub oracles: Vec<OracleVerdict>,
    pub final_summary: AbstractWorldSummary,
}

impl SimulationTrace {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seed: u64,
        generator_version: impl Into<String>,
        profile: impl Into<String>,
        shard: impl Into<String>,
        workload_family: impl Into<String>,
        workload_id: impl Into<String>,
        script_bundle_hash: impl Into<String>,
        expectations: WorkloadExpectations,
        aliases: BTreeMap<String, String>,
        events: Vec<DeliveredBoundary>,
        durable_writes: Vec<crate::store::CheckpointWriteEvent>,
        oracle: OracleVerdict,
        oracles: Vec<OracleVerdict>,
        final_summary: AbstractWorldSummary,
    ) -> Self {
        Self {
            schema: TRACE_SCHEMA.to_string(),
            seed,
            generator_version: generator_version.into(),
            profile: profile.into(),
            shard: shard.into(),
            workload_family: workload_family.into(),
            workload_id: workload_id.into(),
            script_bundle_hash: script_bundle_hash.into(),
            expectations,
            aliases,
            events,
            durable_writes,
            oracle,
            oracles,
            final_summary,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraceEventLine {
    pub schema: String,
    pub trace_alias: String,
    pub seed: u64,
    pub profile: String,
    pub event: DeliveredBoundary,
}

impl TraceEventLine {
    pub fn new(
        trace_alias: impl Into<String>,
        seed: u64,
        profile: impl Into<String>,
        event: DeliveredBoundary,
    ) -> Self {
        Self {
            schema: TRACE_EVENT_LINE_SCHEMA.to_string(),
            trace_alias: trace_alias.into(),
            seed,
            profile: profile.into(),
            event,
        }
    }
}

/// Evidence that replay re-verified the real-runtime invariant facts that the
/// boundary-equality normalization strips out (session-graph acyclicity, the
/// single-active-agent-frame invariant, and usage monotonicity). The counts
/// prove the re-verification actually ran; replay fails before a report is built
/// if any recorded fact is internally inconsistent or reveals a violation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeInvariantReverification {
    pub schema: String,
    pub reverified_turn_count: usize,
    pub graph_invariant_checks: usize,
    pub agent_frame_invariant_checks: usize,
    pub usage_invariant_checks: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplayReport {
    pub schema: String,
    pub trace_path: PathBuf,
    pub terminal_verdict: OracleVerdict,
    pub delivered_event_count: usize,
    pub delivered_boundary_sequence: Vec<String>,
    pub final_summary: AbstractWorldSummary,
    #[serde(default)]
    pub runtime_invariant_reverification: RuntimeInvariantReverification,
}

impl ReplayReport {
    pub fn new(
        trace_path: impl Into<PathBuf>,
        terminal_verdict: OracleVerdict,
        delivered_boundary_sequence: Vec<String>,
        final_summary: AbstractWorldSummary,
        runtime_invariant_reverification: RuntimeInvariantReverification,
    ) -> Self {
        Self {
            schema: REPLAY_REPORT_SCHEMA.to_string(),
            trace_path: trace_path.into(),
            delivered_event_count: delivered_boundary_sequence.len(),
            delivered_boundary_sequence,
            terminal_verdict,
            final_summary,
            runtime_invariant_reverification,
        }
    }
}

#[derive(Debug)]
pub enum TraceIoError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Integrity(String),
}

impl fmt::Display for TraceIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "trace I/O failed: {err}"),
            Self::Json(err) => write!(f, "trace JSON failed: {err}"),
            Self::Integrity(message) => write!(f, "trace integrity check failed: {message}"),
        }
    }
}

impl std::error::Error for TraceIoError {}

impl From<std::io::Error> for TraceIoError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for TraceIoError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub fn write_trace(path: &Path, trace: &SimulationTrace) -> Result<(), TraceIoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(trace)?)?;
    Ok(())
}

pub fn read_trace(path: &Path) -> Result<SimulationTrace, TraceIoError> {
    let body = std::fs::read(path)?;
    let trace: SimulationTrace = serde_json::from_slice(&body)?;
    verify_trace_integrity(&trace)?;
    Ok(trace)
}

/// At-rest integrity gate for a deserialized trace: the schema must match and the
/// embedded provenance hashes (`workload_id` and `script_bundle_hash`) must be
/// well-formed sha256 hex digests. A truncated, corrupted, or hash-stripped trace
/// is rejected at read time rather than silently replayed.
fn verify_trace_integrity(trace: &SimulationTrace) -> Result<(), TraceIoError> {
    if trace.schema != TRACE_SCHEMA {
        return Err(TraceIoError::Integrity(format!(
            "expected schema `{TRACE_SCHEMA}`, got `{}`",
            trace.schema
        )));
    }
    // `workload_id` is always a deterministic sha256 of (seed, profile, generator
    // version, planned boundaries); a non-hex/wrong-length value means the trace
    // was truncated or its provenance was stripped/corrupted.
    if !is_sha256_hex(&trace.workload_id) {
        return Err(TraceIoError::Integrity(format!(
            "workload_id `{}` is not a 64-char sha256 hex digest",
            trace.workload_id
        )));
    }
    // The script bundle hash must be present (a stripped bundle hash is rejected).
    // It is not required to be a 64-char digest so in-memory fixture traces can
    // carry a labelled placeholder bundle id.
    if trace.script_bundle_hash.trim().is_empty() {
        return Err(TraceIoError::Integrity(
            "script_bundle_hash is empty; the trace's provider bundle provenance was stripped"
                .to_string(),
        ));
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn write_event_lines(path: &Path, events: &[TraceEventLine]) -> Result<String, TraceIoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = Vec::new();
    for event in events {
        body.extend_from_slice(&serde_json::to_vec(event)?);
        body.push(b'\n');
    }
    std::fs::write(path, &body)?;
    Ok(hex_digest(&sha256(&body)))
}

pub fn write_replay_report(path: &Path, report: &ReplayReport) -> Result<String, TraceIoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(report)?;
    std::fs::write(path, &body)?;
    Ok(hex_digest(&sha256(&body)))
}

pub fn summary_digest(
    session_count: usize,
    total_events: usize,
    sessions: &[SessionAbstractSummary],
    durable_effects: &[DurableEffectAbstractSummary],
    workers: &[WorkerAbstractSummary],
) -> String {
    let value = serde_json::json!({
        "session_count": session_count,
        "total_events": total_events,
        "sessions": sessions,
        "durable_effects": durable_effects,
        "workers": workers,
    });
    hex_digest(&sha256(value.to_string().as_bytes()))
}

pub fn value_digest(value: &Value) -> String {
    hex_digest(&sha256(value.to_string().as_bytes()))
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
